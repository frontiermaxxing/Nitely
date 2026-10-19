//! Local machine history. One OS file lock elects a recorder across all Nitely clients.
//! This database is deliberately separate from the issue/coordination database.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::time::{Duration, Instant};

#[cfg(windows)]
#[path = "performance_windows.rs"]
mod native;

pub const CADENCE_MS: u64 = 5_000;
pub const MAX_DATABASE_BYTES: u64 = 256 * 1024 * 1024;
/// Increment when collection/schema behavior changes. New clients retire older collectors.
pub const RECORDER_VERSION: i64 = 3;

pub(crate) fn boot_id() -> Option<String> {
    #[cfg(windows)] { native::boot_id() }
    #[cfg(not(windows))] { None }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Metrics {
    pub cpu_percent: Option<f64>,
    pub physical_total_bytes: Option<u64>,
    pub physical_available_bytes: Option<u64>,
    pub commit_bytes: Option<u64>,
    pub commit_limit_bytes: Option<u64>,
    pub kernel_paged_bytes: Option<u64>,
    pub kernel_nonpaged_bytes: Option<u64>,
    pub pages_input_per_sec: Option<f64>,
    pub pages_output_per_sec: Option<f64>,
    pub disk_read_bytes_per_sec: Option<f64>,
    pub disk_write_bytes_per_sec: Option<f64>,
    pub disk_latency_seconds: Option<f64>,
    pub disk_queue: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub wall_ms: i64,
    pub monotonic_ms: u64,
    /// OS boot identity when available, never inferred from a wall-clock subtraction.
    pub boot: Option<String>,
    pub session: String,
    pub metrics: Metrics,
    pub collection_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Minute {
    pub first_wall_ms: i64,
    pub first_monotonic_ms: u64,
    pub last: Sample,
    pub samples: u32,
    pub min_available_bytes: Option<u64>,
    /// Maxima over observed values; missing throughout stays null. Preserve short pressure
    /// spikes after detailed samples expire, including kernel pools, paging and disk latency.
    pub peaks: Metrics,
}
impl Minute {
    pub(crate) fn new(sample: &Sample) -> Self {
        Self { first_wall_ms: sample.wall_ms, first_monotonic_ms: sample.monotonic_ms, last: sample.clone(), samples: 1,
            min_available_bytes: sample.metrics.physical_available_bytes,
            peaks: sample.metrics.clone() }
    }
    pub(crate) fn add(&mut self, sample: &Sample) {
        self.last = sample.clone();
        self.samples += 1;
        if let Some(v) = sample.metrics.physical_available_bytes { self.min_available_bytes = Some(self.min_available_bytes.map_or(v, |old| old.min(v))); }
        macro_rules! peak { ($($field:ident),*) => { $(if let Some(v) = sample.metrics.$field { self.peaks.$field = Some(self.peaks.$field.map_or(v, |old| old.max(v))); })* }; }
        peak!(cpu_percent, physical_total_bytes, physical_available_bytes, commit_bytes, commit_limit_bytes, kernel_paged_bytes, kernel_nonpaged_bytes,
            pages_input_per_sec, pages_output_per_sec, disk_read_bytes_per_sec, disk_write_bytes_per_sec, disk_latency_seconds, disk_queue);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    pub detailed_hours: u32,
    pub summary_days: u32,
}
impl Default for Settings {
    fn default() -> Self { Self { enabled: true, detailed_hours: 24, summary_days: 30 } }
}
impl Settings {
    fn validate(&self) -> Result<()> {
        if !(1..=24).contains(&self.detailed_hours) || !(1..=30).contains(&self.summary_days) { bail!("history retention must be 1–24 detailed hours and 1–30 summary days"); }
        Ok(())
    }
}

pub fn directory() -> PathBuf { crate::paths::data_dir().join("performance") }
pub fn wall_ms() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64 }

/// Lifetime ownership, released by the OS even after a crash. Never delete this lock file.
pub fn acquire(dir: &Path) -> Result<Option<std::fs::File>> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("recorder.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub struct Store { pub(crate) db: Connection }
impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let db = Connection::open(dir.join("history.sqlite"))?;
        db.busy_timeout(Duration::from_millis(250))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA max_page_count=65536;
            CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS recorder_version(id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS samples(id INTEGER PRIMARY KEY, wall_ms INTEGER NOT NULL, json TEXT NOT NULL CHECK(length(json)<=8192));
            CREATE TABLE IF NOT EXISTS minutes(id INTEGER PRIMARY KEY, wall_ms INTEGER NOT NULL, json TEXT NOT NULL CHECK(length(json)<=24576));
            CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY, session TEXT NOT NULL, boot TEXT, started_ms INTEGER NOT NULL, stopped_ms INTEGER);
            CREATE INDEX IF NOT EXISTS samples_time ON samples(wall_ms);
            CREATE INDEX IF NOT EXISTS minutes_time ON minutes(wall_ms);")?;
        crate::performance_events::schema(&db)?;
        db.execute("INSERT INTO recorder_version VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET version=excluded.version WHERE excluded.version>version", [RECORDER_VERSION])?;
        crate::performance_attribution::schema(&db)?;
        Ok(Self { db })
    }
    /// Queries never initialize a store, collect a sample, or contend for recorder ownership.
    /// A crashed writer's hot journal is recovered before returning a read-only connection.
    pub fn read(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join("history.sqlite");
        if !path.exists() { return Ok(None); }
        let open_read = || -> rusqlite::Result<Connection> {
            let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(Duration::from_millis(250))?;
            // Opening alone is lazy: this read detects recovery before any query escapes.
            db.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))?;
            Ok(db)
        };
        let db = match open_read() {
            Ok(db) => db,
            Err(e) if e.sqlite_error().is_some_and(|e| e.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK) => {
                // No CREATE flag, schema initialization, or version announcement. SQLite
                // restores the last committed pages under its own recovery lock.
                let recovery = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
                recovery.busy_timeout(Duration::from_millis(250))?;
                recovery.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))?;
                drop(recovery);
                open_read()?
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Self { db }))
    }
    pub fn settings(&self) -> Result<Settings> {
        let json: Option<String> = self.db.query_row("SELECT json FROM settings WHERE id=1", [], |r| r.get(0)).optional()?;
        let settings: Settings = json.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
        settings.validate()?;
        Ok(settings)
    }
    pub fn configure(&self, settings: &Settings) -> Result<()> {
        if !self.current_version()? { bail!("a newer Nitely performance recorder is installed; use the newer app or CLI"); }
        settings.validate()?;
        self.db.execute("INSERT OR REPLACE INTO settings VALUES(1,?1)", [serde_json::to_string(settings)?])?;
        Ok(())
    }
    pub fn start_session(&self, session: &str, boot: Option<&str>, at: i64) -> Result<()> {
        self.db.execute("INSERT INTO sessions(session,boot,started_ms) VALUES(?1,?2,?3)", params![session, boot, at])?;
        self.db.execute("DELETE FROM sessions WHERE id NOT IN (SELECT id FROM sessions ORDER BY id DESC LIMIT 4096)", [])?;
        Ok(())
    }
    pub fn stop_session(&self, session: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET stopped_ms=?1 WHERE session=?2", params![wall_ms(), session])?;
        Ok(())
    }
    /// The sample and its minute summary commit together. SQLite recovers an interrupted write;
    /// malformed individual JSON records are skipped by readers without discarding older history.
    pub(crate) fn append(&mut self, sample: &Sample, minute: &Minute, minute_id: Option<i64>, settings: &Settings) -> Result<i64> {
        let tx = self.db.transaction()?;
        // Count caps are clock-independent; wall time alone cannot make storage unbounded.
        let detailed = i64::from(settings.detailed_hours) * 720;
        let summaries = i64::from(settings.summary_days) * 1440;
        tx.execute("DELETE FROM samples WHERE wall_ms < ?1", [sample.wall_ms.saturating_sub(i64::from(settings.detailed_hours) * 3_600_000)])?;
        tx.execute("DELETE FROM minutes WHERE wall_ms < ?1", [sample.wall_ms.saturating_sub(i64::from(settings.summary_days) * 86_400_000)])?;
        tx.execute("DELETE FROM performance_event WHERE wall_ms < ?1", [sample.wall_ms.saturating_sub(i64::from(settings.summary_days) * 86_400_000)])?;
        // IDs grow while rows remain; gaps can only retain fewer rows. Indexed cutoffs avoid
        // scanning 60,000 retained JSON records on every five-second sample.
        tx.execute("DELETE FROM samples WHERE id <= (SELECT max(id) FROM samples)-?1", [detailed - 1])?;
        tx.execute("DELETE FROM minutes WHERE id <= (SELECT max(id) FROM minutes)-?1", [summaries - 1])?;
        tx.execute("INSERT INTO samples(wall_ms,json) VALUES(?1,?2)", params![sample.wall_ms, serde_json::to_string(sample)?])?;
        let json = serde_json::to_string(minute)?;
        let id = if let Some(id) = minute_id {
            tx.execute("UPDATE minutes SET wall_ms=?1,json=?2 WHERE id=?3", params![minute.last.wall_ms, json, id])?;
            id
        } else {
            tx.execute("INSERT INTO minutes(wall_ms,json) VALUES(?1,?2)", params![minute.last.wall_ms, json])?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(id)
    }
    pub fn samples(&self, from: i64, to: i64, limit: usize) -> Result<Vec<Sample>> {
        self.read_json("SELECT json FROM samples WHERE wall_ms>=?1 AND wall_ms<=?2 ORDER BY id DESC LIMIT ?3", from, to, limit)
    }
    pub fn minutes(&self, from: i64, to: i64, limit: usize) -> Result<Vec<Minute>> {
        self.read_json("SELECT json FROM minutes WHERE wall_ms>=?1 AND wall_ms<=?2 ORDER BY id DESC LIMIT ?3", from, to, limit)
    }
    fn read_json<T: serde::de::DeserializeOwned>(&self, sql: &str, from: i64, to: i64, limit: usize) -> Result<Vec<T>> {
        if from > to || limit == 0 || limit > 43200 { bail!("invalid history range or limit (1–43200)"); }
        let mut stmt = self.db.prepare(sql)?;
        // A corrupt tail record must not hide the last complete sample. Overscan is bounded.
        let rows = stmt.query_map(params![from, to, (limit + 64) as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows { if let Ok(value) = serde_json::from_str(&row?) { out.push(value); if out.len() == limit { break; } } }
        Ok(out)
    }
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.db.prepare("SELECT session,boot,started_ms,stopped_ms FROM sessions ORDER BY id DESC LIMIT 4096")?;
        let rows = stmt.query_map([], |r| Ok(Session { session: r.get(0)?, boot: r.get(1)?, started_ms: r.get(2)?, stopped_ms: r.get(3)? }))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    fn clear(&self) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, rusqlite::TransactionBehavior::Immediate)?;
        if !self.current_version()? { bail!("use the newer Nitely to clear performance history"); }
        if self.settings()?.enabled { bail!("pause performance recording before deleting history"); }
        tx.execute_batch("DELETE FROM samples; DELETE FROM minutes; DELETE FROM sessions; DELETE FROM attribution; DELETE FROM attribution_minutes; DELETE FROM workloads; DELETE FROM performance_event;")?;
        tx.commit()?;
        self.db.execute_batch("VACUUM;")?;
        Ok(())
    }
    pub(crate) fn current_version(&self) -> Result<bool> { Ok(self.db.query_row("SELECT version FROM recorder_version WHERE id=1", [], |r| r.get::<_, i64>(0))? == RECORDER_VERSION) }
}

/// Paused deletion is serialized with the recorder, so an in-flight sample cannot repopulate it.
pub fn clear_history(dir: &Path) -> Result<()> {
    let store = Store::open(dir)?;
    if store.settings()?.enabled { bail!("pause performance recording before deleting history"); }
    let _owner = acquire(dir)?.ok_or_else(|| anyhow::anyhow!("recorder is stopping; retry after five seconds"))?;
    store.clear()?;
    let _ = std::fs::remove_file(dir.join("last-error.txt"));
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session { pub session: String, pub boot: Option<String>, pub started_ms: i64, pub stopped_ms: Option<i64> }

pub struct Collector {
    #[cfg(not(windows))]
    started: Instant,
    session: String,
    #[cfg(windows)]
    native: native::Counters,
}
impl Default for Collector { fn default() -> Self { Self::new() } }
impl Collector {
    pub fn new() -> Self {
        Self { #[cfg(not(windows))] started: Instant::now(), session: format!("{}-{}", std::process::id(), crate::paths::unique_temp("session", "").file_name().unwrap().to_string_lossy()), #[cfg(windows)] native: native::Counters::new() }
    }
    pub fn sample(&mut self) -> Sample {
        let at = Instant::now();
        #[cfg(windows)]
        let (metrics, boot, monotonic_ms) = self.native.read();
        #[cfg(not(windows))]
        let (metrics, boot, monotonic_ms) = {
            let memory = crate::machine::memory();
            (Metrics { physical_total_bytes: memory.map(|m| m.total), physical_available_bytes: memory.map(|m| m.available), ..Metrics::default() }, None, self.started.elapsed().as_millis() as u64)
        };
        Sample { wall_ms: wall_ms(), monotonic_ms, boot, session: self.session.clone(), metrics, collection_ms: at.elapsed().as_millis() as u64 }
    }
}

/// Held by the desktop or a long-lived CLI/Crew/MCP process. Losing one client lets another
/// acquire the OS lock. No extra subprocess, login task, shell scan, or process arguments.
pub struct Recording { stop: Arc<AtomicBool>, thread: Option<std::thread::JoinHandle<()>> }
impl Drop for Recording {
    fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); if let Some(thread) = self.thread.take() { let _ = thread.join(); } }
}
pub fn background(dir: PathBuf) -> Recording {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let error_dir = dir.clone();
    let thread = std::thread::Builder::new().name("performance-recorder".into()).spawn(move || {
        // Announce before acquiring ownership, so an older owner can relinquish its lock.
        // Failures stay isolated from the app/agent and are retried below.
        let mut announced = false;
        while !stopping.load(Ordering::Relaxed) {
            let result = (|| {
                if !announced {
                    Store::open(&dir)?;
                    announced = true;
                }
                crate::performance_attribution::ensure_service(&dir);
                record(&dir, &stopping)
            })();
            if let Err(error) = result {
                // One bounded diagnostic, not a growing log. A full disk may prevent even this.
                let text = format!("{}: {error:#}", wall_ms());
                let _ = std::fs::write(dir.join("last-error.txt"), text.chars().take(2048).collect::<String>());
            }
            wait(&stopping, Duration::from_millis(CADENCE_MS));
        }
    });
    let thread = match thread {
        Ok(thread) => Some(thread),
        Err(error) => {
            let _ = std::fs::create_dir_all(&error_dir);
            let _ = std::fs::write(error_dir.join("last-error.txt"), format!("{}: recorder thread could not start: {error}", wall_ms()));
            None
        }
    };
    Recording { stop, thread }
}
fn wait(stop: &AtomicBool, duration: Duration) {
    let until = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) && Instant::now() < until { std::thread::sleep(Duration::from_millis(100)); }
}
pub fn record(dir: &Path, stop: &AtomicBool) -> Result<()> {
    let Some(_owner) = acquire(dir)? else { return Ok(()) };
    let mut store = Store::open(dir)?;
    if !store.current_version()? || !store.settings()?.enabled { return Ok(()); }
    crate::performance_attribution::ensure_service(dir);
    let mut collector = Collector::new();
    let mut attribution = crate::performance_attribution::Sampler::default();
    let mut minute: Option<Minute> = None;
    let mut minute_id = None;
    let first = collector.sample();
    store.start_session(&first.session, first.boot.as_deref(), first.wall_ms)?;
    let mut sample = first;
    while !stop.load(Ordering::Relaxed) {
        let settings = store.settings()?;
        if !store.current_version()? || !settings.enabled { break; }
        let mut attributed = attribution.collect(&store,&sample)?;
        sample.collection_ms = sample.collection_ms.saturating_add(attributed.collection_ms);
        if minute.as_ref().is_none_or(|m| sample.monotonic_ms.saturating_sub(m.first_monotonic_ms) >= 60_000
            || (sample.wall_ms - m.last.wall_ms - sample.monotonic_ms.saturating_sub(m.last.monotonic_ms) as i64).unsigned_abs() > 2_000) {
            minute = Some(Minute::new(&sample)); minute_id = None;
        } else { minute.as_mut().unwrap().add(&sample); }
        let new_minute = minute_id.is_none();
        minute_id = Some(store.append(&sample, minute.as_ref().unwrap(), minute_id, &settings).context("saving performance sample")?);
        store.append_attribution(&mut attributed,new_minute).context("saving workload attribution")?;
        wait(stop, Duration::from_millis(CADENCE_MS));
        sample = collector.sample();
    }
    store.stop_session(&collector.session)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp { fn new() -> Self { Self(crate::paths::unique_temp("nitely-performance", "")) } }
    impl Drop for Temp { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    fn sample(at: i64) -> Sample {
        Sample { wall_ms: at, monotonic_ms: at.max(0) as u64, boot: Some("boot-a".into()), session: "session-a".into(), metrics: Metrics::default(), collection_ms: 0 }
    }
    fn save(store: &mut Store, at: i64, settings: &Settings) -> Result<()> {
        let sample = sample(at);
        store.append(&sample, &Minute::new(&sample), None, settings)?;
        Ok(())
    }
    #[test]
    fn competing_clients_have_one_owner_and_can_take_over() {
        let temp = Temp::new();
        let first = acquire(&temp.0).unwrap().unwrap();
        let dir = temp.0.clone();
        assert!(std::thread::spawn(move || acquire(&dir).unwrap().is_none()).join().unwrap());
        drop(first);
        assert!(acquire(&temp.0).unwrap().is_some());
    }
    #[test]
    fn interrupted_transaction_and_malformed_last_record_keep_prior_history() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        save(&mut store, 1, &Settings::default()).unwrap();
        { let tx = store.db.transaction().unwrap(); tx.execute("INSERT INTO samples(wall_ms,json) VALUES(2,'partial')", []).unwrap(); }
        drop(store);
        let store = Store::open(&temp.0).unwrap();
        assert_eq!(store.samples(0, 10, 10).unwrap().len(), 1);
        store.db.execute("INSERT INTO samples(wall_ms,json) VALUES(3,'broken')", []).unwrap();
        let samples = store.samples(0, 10, 10).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].wall_ms, 1);
        assert_eq!(samples[0].metrics.cpu_percent, None);
        assert_eq!(store.samples(0, 10, 1).unwrap()[0].wall_ms, 1);
    }
    #[test]
    fn hot_journal_child() {
        let Some(dir) = std::env::var_os("NITELY_TEST_HOT_JOURNAL_DIR") else { return; };
        let db = Connection::open(PathBuf::from(dir).join("history.sqlite")).unwrap();
        db.execute_batch("PRAGMA cache_size=1; BEGIN IMMEDIATE;").unwrap();
        db.execute("UPDATE samples SET json=?1", ["x".repeat(7000)]).unwrap();
        // Deliberately bypass destructors after dirty pages spill, like process termination.
        std::process::exit(0);
    }
    #[test]
    fn read_recovers_a_hot_journal_without_starting_or_upgrading_the_recorder() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        let json = serde_json::to_string(&sample(1)).unwrap();
        let tx = store.db.transaction().unwrap();
        for _ in 0..64 { tx.execute("INSERT INTO samples(wall_ms,json) VALUES(1,?1)", [&json]).unwrap(); }
        tx.commit().unwrap();
        store.configure(&Settings { enabled: false, ..Default::default() }).unwrap();
        store.db.execute("UPDATE recorder_version SET version=?1", [RECORDER_VERSION + 1]).unwrap();
        drop(store);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "performance::tests::hot_journal_child", "--nocapture"])
            .env("NITELY_TEST_HOT_JOURNAL_DIR", &temp.0).output().unwrap();
        assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
        let readonly = Connection::open_with_flags(temp.0.join("history.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let error = readonly.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0)).unwrap_err();
        assert_eq!(error.sqlite_error().unwrap().extended_code, rusqlite::ffi::SQLITE_READONLY_ROLLBACK, "fixture must require recovery");
        drop(readonly);
        let recovered = Store::read(&temp.0).unwrap().unwrap();
        assert_eq!(recovered.samples(0, 10, 100).unwrap().len(), 64);
        assert!(!recovered.settings().unwrap().enabled);
        assert!(!recovered.current_version().unwrap(), "reading must not announce a recorder version");
        assert!(recovered.sessions().unwrap().is_empty());
        assert!(recovered.db.execute("DELETE FROM samples", []).is_err(), "returned connection remains read-only");
    }
    #[test]
    fn full_store_rejects_whole_write_without_losing_history() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        save(&mut store, 1, &Settings::default()).unwrap();
        let pages: i64 = store.db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
        store.db.pragma_update(None, "max_page_count", pages).unwrap();
        let mut failed = false;
        for at in 2..1000 {
            if save(&mut store, at, &Settings::default()).is_err() { failed = true; break; }
        }
        assert!(failed, "exercise SQLITE_FULL");
        assert_eq!(store.samples(1, 1, 1).unwrap().len(), 1);
        assert_eq!(store.db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)).unwrap(), "ok");
    }
    #[test]
    fn retention_is_bounded_even_when_clock_moves_backwards() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        // Batch fixtures before exercising retention (no hundreds of fsync calls in a unit test).
        let tx = store.db.transaction().unwrap();
        let json = serde_json::to_string(&sample(100_000)).unwrap();
        for _ in 0..750 { tx.execute("INSERT INTO samples(wall_ms,json) VALUES(100000,?1)", [&json]).unwrap(); }
        tx.commit().unwrap();
        save(&mut store, 1, &Settings { detailed_hours: 1, ..Settings::default() }).unwrap();
        assert_eq!(store.samples(0, i64::MAX, 1000).unwrap().len(), 720);
        save(&mut store, 4_000_000, &Settings { detailed_hours: 1, ..Settings::default() }).unwrap();
        assert_eq!(store.samples(0, i64::MAX, 1000).unwrap().len(), 1);
        store.db.execute("INSERT INTO performance_event(wall_ms,project,run,kind,json) VALUES(1,'APP',1,'started','{}')", []).unwrap();
        save(&mut store, 86_400_002, &Settings { summary_days: 1, ..Default::default() }).unwrap();
        let events: i64 = store.db.query_row("SELECT count(*) FROM performance_event", [], |r| r.get(0)).unwrap();
        assert_eq!(events, 0, "periodic sampling expires events even without a new workload transition");
    }
    #[test]
    fn stop_and_resume_preserve_sessions_and_clear_requires_pause() {
        let temp = Temp::new();
        let mut store = Store::open(&temp.0).unwrap();
        store.start_session("a", Some("boot-a"), 1).unwrap();
        save(&mut store, 1, &Settings::default()).unwrap();
        store.stop_session("a").unwrap();
        store.start_session("b", Some("boot-b"), 2).unwrap();
        let sessions = store.sessions().unwrap();
        assert_eq!(sessions[0].stopped_ms, None);
        assert!(sessions[1].stopped_ms.is_some());
        assert!(store.clear().is_err());
        store.configure(&Settings { enabled: false, ..Settings::default() }).unwrap();
        store.clear().unwrap();
        assert!(store.samples(0, i64::MAX, 1).unwrap().is_empty());
        assert!(!store.settings().unwrap().enabled);
        assert!(store.configure(&Settings { summary_days: 31, ..Settings::default() }).is_err());
    }
    #[test]
    fn minute_extrema_keep_unknowns_unknown_and_capture_pressure() {
        let mut first = sample(1);
        let mut minute = Minute::new(&first);
        assert_eq!(minute.peaks.commit_bytes, None);
        first.metrics.commit_bytes = Some(100);
        first.metrics.physical_available_bytes = Some(5);
        minute.add(&first);
        first.metrics.commit_bytes = Some(90);
        first.metrics.physical_available_bytes = Some(10);
        minute.add(&first);
        assert_eq!(minute.peaks.commit_bytes, Some(100));
        assert_eq!(minute.min_available_bytes, Some(5));
    }
    #[test]
    fn newer_recorder_generation_cannot_be_downgraded_by_an_old_client() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.db.execute("UPDATE recorder_version SET version=?1", [RECORDER_VERSION + 1]).unwrap();
        let old = Store::open(&temp.0).unwrap();
        assert!(!old.current_version().unwrap());
        assert!(old.configure(&Settings::default()).is_err());
        assert!(old.clear().is_err());
        assert!(record(&temp.0, &AtomicBool::new(false)).is_ok());
        assert!(old.sessions().unwrap().is_empty());
    }
    #[test]
    fn a_busy_database_does_not_lose_the_new_collectors_version_announcement() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.db.execute("UPDATE recorder_version SET version=0", []).unwrap();
        let _old_owner = acquire(&temp.0).unwrap().unwrap();
        store.db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let recording = background(temp.0.clone());
        std::thread::sleep(Duration::from_millis(400));
        store.db.execute_batch("COMMIT").unwrap();
        let deadline = Instant::now() + Duration::from_secs(7);
        while !store.current_version().unwrap() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(20)); }
        assert!(store.current_version().unwrap(), "retry announcement before trying the old owner's lock");
        drop(recording);
        assert!(store.sessions().unwrap().is_empty(), "contender never collected while old owner held lock");
    }
    #[test]
    #[ignore = "manual bounded native collection, process I/O and retention-size measurement"]
    fn recorder_budget_measurement() {
        #[cfg(windows)]
        #[repr(C)]
        #[derive(Default)]
        struct IoCounters {
            read_operations: u64,
            write_operations: u64,
            other_operations: u64,
            read_bytes: u64,
            write_bytes: u64,
            other_bytes: u64,
        }
        #[cfg(windows)]
        fn process_io() -> IoCounters {
            #[link(name = "kernel32")]
            extern "system" {
                fn GetCurrentProcess() -> *mut std::ffi::c_void;
                fn GetProcessIoCounters(process: *mut std::ffi::c_void, counters: *mut [u64; 6]) -> i32;
            }
            let mut counters = IoCounters::default();
            // SAFETY: the current-process pseudo-handle is valid, and the writable buffer has
            // the six consecutive ULONGLONG fields of Windows IO_COUNTERS in C layout.
            let ok = unsafe { GetProcessIoCounters(GetCurrentProcess(), (&mut counters as *mut IoCounters).cast()) };
            assert_ne!(ok, 0, "process I/O counters: {}", std::io::Error::last_os_error());
            counters
        }
        #[cfg(windows)]
        fn process_cpu_ticks() -> u64 {
            #[link(name = "kernel32")]
            extern "system" {
                fn GetCurrentProcess() -> *mut std::ffi::c_void;
                fn GetProcessTimes(process: *mut std::ffi::c_void, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
            }
            let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
            // SAFETY: current-process pseudo-handle and four writable, aligned FILETIME-sized
            // buffers; Windows returns accumulated kernel/user CPU time in 100ns units.
            let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user) };
            assert_ne!(ok, 0, "process CPU counters: {}", std::io::Error::last_os_error());
            kernel.checked_add(user).unwrap()
        }
        #[cfg(windows)]
        let resident_before = native::resident_bytes();
        let temp = Temp::new();
        let mut collector = Collector::new();
        let mut store = Store::open(&temp.0).unwrap();
        // Seed maximum retention in one transaction, measuring steady-state pruning as well
        // as cold collection. The fixture never touches the person's telemetry.
        let mut seed = collector.sample();
        seed.wall_ms = wall_ms();
        let sample_json = serde_json::to_string(&seed).unwrap();
        let minute_json = serde_json::to_string(&Minute::new(&seed)).unwrap();
        {
            let tx = store.db.transaction().unwrap();
            {
                let mut details = tx.prepare("INSERT INTO samples(wall_ms,json) VALUES(?1,?2)").unwrap();
                for _ in 0..17280 { details.execute(params![seed.wall_ms, sample_json]).unwrap(); }
                let mut minutes = tx.prepare("INSERT INTO minutes(wall_ms,json) VALUES(?1,?2)").unwrap();
                for _ in 0..43200 { minutes.execute(params![seed.wall_ms, minute_json]).unwrap(); }
            }
            tx.commit().unwrap();
        }
        assert_eq!(store.db.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "benchmark FULL durability, not buffered commits");
        // Exclude fixture construction; include native counter collection, retention scans,
        // rollback journals and database writes for twelve independently durable cycles.
        #[cfg(windows)]
        let cpu_before = process_cpu_ticks();
        #[cfg(windows)]
        let io_before = process_io();
        let start = Instant::now();
        for _ in 0..12 {
            let sample = collector.sample();
            store.append(&sample, &Minute::new(&sample), None, &Settings::default()).unwrap();
        }
        let elapsed = start.elapsed();
        #[cfg(windows)]
        let io_after = process_io();
        #[cfg(windows)]
        let cpu = Duration::from_nanos(process_cpu_ticks().checked_sub(cpu_before).unwrap().checked_mul(100).unwrap());
        let sample = collector.sample();
        let detail = serde_json::to_string(&sample).unwrap().len();
        let minute = serde_json::to_string(&Minute::new(&sample)).unwrap().len();
        let disk = std::fs::metadata(temp.0.join("history.sqlite")).unwrap().len();
        eprintln!("12 native collections+writes at full retention: {elapsed:?}; sample JSON {detail} bytes; minute JSON {minute} bytes; database {disk} bytes; max retained payload {} bytes", detail * 17280 + minute * 43200);
        #[cfg(windows)]
        {
            eprintln!("12-cycle process CPU: {cpu:?}; elapsed wall time: {elapsed:?}");
            let read = io_after.read_bytes.checked_sub(io_before.read_bytes).unwrap();
            let written = io_after.write_bytes.checked_sub(io_before.write_bytes).unwrap();
            let other = io_after.other_bytes.checked_sub(io_before.other_bytes).unwrap();
            let reads = io_after.read_operations.checked_sub(io_before.read_operations).unwrap();
            let writes = io_after.write_operations.checked_sub(io_before.write_operations).unwrap();
            let others = io_after.other_operations.checked_sub(io_before.other_operations).unwrap();
            eprintln!("12-cycle process I/O: read {read} bytes/{reads} operations; written {written} bytes/{writes} operations; other {other} bytes/{others} operations (one minute at the five-second cadence)");
            // At normal cadence these allow at most 205 KiB/s reads and 51 KiB/s writes,
            // with room for FULL-sync journal amplification. These are process transfer
            // volumes, not physical-disk traffic, flush counts or storage latency.
            assert!(written > 0 && writes > 0, "measure actual durable database writes");
            assert!(read <= 12 * 1024 * 1024, "recorder process-read budget: {read} bytes per twelve cycles");
            assert!(written <= 3 * 1024 * 1024, "recorder process-write budget: {written} bytes per twelve cycles");
            assert!(cpu < Duration::from_millis(600), "recorder CPU must stay below 1% of one core over twelve five-second intervals: {cpu:?}");
        }
        #[cfg(not(windows))]
        eprintln!("process CPU and I/O budgets unavailable on this platform; measured on Windows only");
        // FULL-sync storage waits and scheduling delays are elapsed time, not process CPU.
        // Keep them visible and require throughput to fit the twelve actual sample intervals.
        assert!(elapsed < Duration::from_secs(60), "twelve collection/commit cycles must fit twelve five-second sampling intervals: {elapsed:?}");
        assert!(detail * 17280 + minute * 43200 < MAX_DATABASE_BYTES as usize);
        #[cfg(windows)]
        if let (Some(before), Some(after)) = (resident_before, native::resident_bytes()) {
            let additional = after.saturating_sub(before);
            eprintln!("additional process resident memory, including full-store fixture: {additional} bytes");
            assert!(additional < 32 * 1024 * 1024, "recorder resident budget");
        }
    }
}
