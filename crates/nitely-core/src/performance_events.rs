//! Small durable workload observations. No command text, paths, output or free-form errors.
use crate::performance::{self, Store};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Started, Ended, WaitingForMemory, MemoryWaitEnded, OutOfMemory, MemoryTrimmed }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub wall_ms: i64,
    #[serde(default)]
    pub boot: Option<String>,
    pub kind: Kind,
    pub project: String,
    pub issue: Option<String>,
    pub run: i64,
    pub memory_cap_bytes: Option<u64>,
    /// For `MemoryTrimmed`: how many idle programs, and the memory available before and after
    /// (memory_trim.rs, NIT-657).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim: Option<crate::memory_trim::Trim>,
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
    record_in_store(&mut store, event)
}
/// Whether an event goes in the history: not under a newer recorder, not while recording is paused,
/// and not when it repeats its run's last kind (NIT-535).
fn keep_event(version: i64, enabled: bool, prior: Option<&str>, kind: &str) -> bool {
    // Each trim is its own operation with its own measurements, repeated or not (NIT-657 review).
    version <= performance::RECORDER_VERSION && enabled && (prior != Some(kind) || kind == "\"memory_trimmed\"")
}
fn record_in_store(store: &mut Store, event: &Event) -> Result<()> {
    let kind = serde_json::to_string(&event.kind)?;
    let tx = store.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Serialize the enabled/version check with pause+clear; an event which waited for a writer
    // must not recreate history after the person paused and deleted it.
    let version: i64 = tx.query_row("SELECT version FROM recorder_version WHERE id=1", [], |r| r.get(0))?;
    let settings: Option<String> = tx.query_row("SELECT json FROM settings WHERE id=1", [], |r| r.get(0)).optional()?;
    let settings: performance::Settings = settings.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
    let prior: Option<String> = tx.query_row("SELECT kind FROM performance_event WHERE run=?1 ORDER BY id DESC LIMIT 1", [event.run], |r| r.get(0)).optional()?;
    if !keep_event(version, settings.enabled, prior.as_deref(), &kind) { return Ok(()); }
    let days = settings.summary_days;
    tx.execute("DELETE FROM performance_event WHERE wall_ms<?1 OR id<=(SELECT max(id) FROM performance_event)-19999", [event.wall_ms.saturating_sub(i64::from(days)*86_400_000)])?;
    tx.execute("INSERT INTO performance_event(wall_ms,project,run,kind,json) VALUES(?1,?2,?3,?4,?5)", params![event.wall_ms,event.project,event.run,kind,serde_json::to_string(event)?])?;
    tx.commit()?;
    Ok(())
}
/// Recorder failures never stop a workload. The event stays with its coordination database,
/// including test/harness homes and lane callers whose child changes NITELY_HOME.
pub fn lane(db: &crate::Db, run: i64, kind: Kind) {
    lane_with(db, run, kind, None)
}
/// A trim of idle programs' memory for waiting run `run` (NIT-657).
pub fn lane_trim(db: &crate::Db, run: i64, trim: crate::memory_trim::Trim) {
    lane_with(db, run, Kind::MemoryTrimmed, Some(trim))
}
fn lane_with(db: &crate::Db, run: i64, kind: Kind, trim: Option<crate::memory_trim::Trim>) {
    let Some(dir) = db.conn.path().filter(|p| !p.is_empty()).and_then(|p| Path::new(p).parent()) else { return };
    let row = db.conn.query_row("SELECT project,issue_key,memory_cap FROM lane_run WHERE id=?1", [run], |r| Ok((r.get::<_, String>(0)?,r.get::<_, Option<String>>(1)?,r.get::<_, Option<i64>>(2)?)));
    if let Ok((project, issue, cap)) = row { let _ = record(&dir.join("performance"), &Event { wall_ms: performance::wall_ms(), boot: performance::boot_id(), kind, project, issue, run, memory_cap_bytes: cap.and_then(|v| u64::try_from(v).ok()), trim }); }
}
pub fn read(store: &Store, from: i64, to: i64, project: Option<&str>, limit: usize) -> Result<(Vec<Event>, bool)> {
    read_for_boot(store, from, to, project, limit, None)
}
pub fn read_for_boot(store: &Store, from: i64, to: i64, project: Option<&str>, limit: usize, boot: Option<&str>) -> Result<(Vec<Event>, bool)> {
    if limit == 0 || limit > 2000 { bail!("event limit must be 1–2000"); }
    let exists: bool = store.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='performance_event')", [], |r| r.get(0))?;
    if !exists { return Ok((vec![], false)); }
    let mut stmt = store.db.prepare("SELECT json FROM performance_event WHERE wall_ms>=?1 AND wall_ms<=?2 AND (?3 IS NULL OR project=?3 COLLATE NOCASE) AND (?5 IS NULL OR CASE WHEN json_valid(json) THEN json_extract(json,'$.boot') END=?5) ORDER BY id DESC LIMIT ?4")?;
    let mut events = Vec::new();
    let mut rows = 0;
    for row in stmt.query_map(params![from,to,project,(limit+1) as i64,boot], |r| r.get::<_, String>(0))? { rows += 1; if let Ok(event) = serde_json::from_str(&row?) { events.push(event); } }
    events.truncate(limit);
    Ok((events, rows > limit))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_event_is_kept_only_when_recording_and_new_for_its_run() {
        let v = performance::RECORDER_VERSION;
        assert!(keep_event(v, true, None, "\"started\""));
        assert!(keep_event(v, true, Some("\"queued\""), "\"started\""));
        assert!(!keep_event(v, true, Some("\"started\""), "\"started\""), "a repeat of the run's last kind");
        assert!(!keep_event(v, false, None, "\"started\""), "paused");
        assert!(!keep_event(v + 1, true, None, "\"started\""), "a newer recorder owns the history");
        assert!(keep_event(v, true, Some("\"memory_trimmed\""), "\"memory_trimmed\""), "a second trim in one wait is recorded too");
        assert_eq!(serde_json::to_string(&Kind::MemoryTrimmed).unwrap(), "\"memory_trimmed\"");
    }
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
        let event = |at, run| Event { wall_ms: at, boot: None, kind: Kind::Started, project: "APP".into(), issue: None, run, memory_cap_bytes: None, trim: None };
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
    #[test]
    fn a_waiting_event_cannot_repopulate_paused_deleted_history() {
        thread_local! { static WAITING: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) }; }
        let dir = crate::paths::unique_temp("performance-event-pause-race", "");
        let mut writer = Store::open(&dir).unwrap();
        let mut person = Store::open(&dir).unwrap();
        let tx = person.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        tx.execute("INSERT INTO settings VALUES(1,?1)", [serde_json::to_string(&performance::Settings { enabled: false, ..Default::default() }).unwrap()]).unwrap();
        tx.execute("DELETE FROM performance_event", []).unwrap();
        let (ready, wait) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            WAITING.with(|signal| *signal.borrow_mut() = Some(ready));
            // Signal only once SQLite actually waits for the person's transaction. This
            // fixture tolerates scheduling/fsync delay without changing production's timeout.
            writer.db.busy_handler(Some(|attempt| {
                WAITING.with(|signal| { if let Some(ready) = signal.borrow_mut().take() { ready.send(()).unwrap(); } });
                std::thread::sleep(std::time::Duration::from_millis(5));
                attempt < 1000
            })).unwrap();
            record_in_store(&mut writer, &Event { wall_ms: performance::wall_ms(), boot: None, kind: Kind::Started, project: "APP".into(), issue: None, run: 1, memory_cap_bytes: None, trim: None }).unwrap();
        });
        wait.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        tx.commit().unwrap();
        thread.join().unwrap();
        assert!(read(&person, 0, i64::MAX, None, 100).unwrap().0.is_empty());
        drop(person);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
