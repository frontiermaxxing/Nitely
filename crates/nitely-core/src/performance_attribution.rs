//! Sampled process attribution. Ownership comes from registered launches, never executable names.
//! Process working sets overlap; their sum is not physical machine RAM. Process I/O is not disk I/O.
use crate::performance::{Sample, Store};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Identity {
    pub boot: String,
    pub pid: u32,
    pub started: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub project: Option<String>,
    pub issue: Option<String>,
    pub run_id: Option<i64>,
    pub job_id: Option<i64>,
    pub worker_id: Option<i64>,
    pub lane: Option<u32>,
    #[serde(default)]
    pub review_batch_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workload {
    pub id: String,
    pub kind: String,
    pub context: Context,
    pub root: Identity,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    /// True when exit was observed by a later sample rather than the launcher.
    #[serde(default)]
    pub end_observed: bool,
    /// Inclusive Job Object counters, kept separate from non-overlapping sampled totals.
    pub job_accounting: Option<crate::procs::Accounting>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub cpu_percent: Option<f64>,
    pub private_commit_bytes: Option<u64>,
    pub working_set_sum_bytes: Option<u64>,
    pub process_read_bytes: Option<u64>,
    pub process_write_bytes: Option<u64>,
    pub processes: u32,
}
impl Usage {
    fn add(&mut self, other: &Self) {
        fn sum(a: &mut Option<u64>, b: Option<u64>) {
            if let Some(b) = b {
                *a = Some(a.unwrap_or(0).saturating_add(b));
            }
        }
        if let Some(v) = other.cpu_percent {
            self.cpu_percent = Some(self.cpu_percent.unwrap_or(0.) + v);
        }
        sum(&mut self.private_commit_bytes, other.private_commit_bytes);
        sum(&mut self.working_set_sum_bytes, other.working_set_sum_bytes);
        sum(&mut self.process_read_bytes, other.process_read_bytes);
        sum(&mut self.process_write_bytes, other.process_write_bytes);
        self.processes += other.processes;
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub enumerated: u32,
    pub measured: u32,
    pub denied: u32,
    pub missing_identity: u32,
    pub partial_metrics: u32,
    pub truncated: bool,
    pub first_sample: bool,
    pub supported: bool,
    /// Polling cannot see every process that starts and exits between samples.
    pub short_lived_partial: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadUsage {
    pub id: String,
    pub context: Context,
    pub kind: String,
    pub usage: Usage,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtherProcess {
    pub basename: String,
    pub identity: Identity,
    pub usage: Usage,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributionSample {
    pub wall_ms: i64,
    pub monotonic_ms: u64,
    pub session: String,
    pub boot: Option<String>,
    pub interval_ms: Option<u64>,
    pub collection_ms: u64,
    pub coverage: Coverage,
    pub resolution: String,
    pub observations: u32,
    pub nitely: Usage,
    pub related: Usage,
    pub other: Usage,
    /// A CPU comparison only. It includes inaccessible/missed processes and sampling skew.
    pub unattributed_cpu_percent: Option<f64>,
    pub workloads: Vec<WorkloadUsage>,
    pub top_other: Vec<OtherProcess>,
}

// Storage uses tuples and references, while query responses keep descriptive field names.
type PackedUsage = (Option<f64>, Option<u64>, Option<u64>, Option<u64>, Option<u64>, u32);
fn pack(u: &Usage) -> PackedUsage {
    (u.cpu_percent, u.private_commit_bytes, u.working_set_sum_bytes, u.process_read_bytes, u.process_write_bytes, u.processes)
}
fn unpack(u: PackedUsage) -> Usage {
    Usage { cpu_percent: u.0, private_commit_bytes: u.1, working_set_sum_bytes: u.2, process_read_bytes: u.3, process_write_bytes: u.4, processes: u.5 }
}
#[derive(Serialize, Deserialize)]
struct Compact {
    q: bool,
    k: u32,
    w: i64,
    m: u64,
    s: String,
    b: Option<String>,
    i: Option<u64>,
    c: u64,
    v: Coverage,
    n: PackedUsage,
    r: PackedUsage,
    o: PackedUsage,
    u: Option<f64>,
    j: Vec<(String, PackedUsage)>,
    p: Vec<(String, u32, String, PackedUsage)>,
}
impl Compact {
    fn from(s: &AttributionSample) -> Self {
        Self {
            q: s.resolution == "minute_peak",
            k: s.observations,
            w: s.wall_ms,
            m: s.monotonic_ms,
            s: s.session.clone(),
            b: s.boot.clone(),
            i: s.interval_ms,
            c: s.collection_ms,
            v: s.coverage.clone(),
            n: pack(&s.nitely),
            r: pack(&s.related),
            o: pack(&s.other),
            u: s.unattributed_cpu_percent,
            j: s.workloads.iter().map(|w| (w.id.clone(), pack(&w.usage))).collect(),
            p: s.top_other.iter().map(|p| (p.basename.clone(), p.identity.pid, p.identity.started.clone(), pack(&p.usage))).collect(),
        }
    }
    fn expand(self, roots: &HashMap<String, Workload>) -> AttributionSample {
        let mut coverage = self.v;
        let workloads = self
            .j
            .into_iter()
            .map(|(id, u)| {
                let w = roots.get(&id);
                if w.is_none() {
                    coverage.truncated = true;
                }
                WorkloadUsage {
                    id,
                    context: w.map(|w| w.context.clone()).unwrap_or_default(),
                    kind: w.map(|w| w.kind.clone()).unwrap_or_else(|| "unavailable".into()),
                    usage: unpack(u),
                }
            })
            .collect();
        let top_other = self
            .p
            .into_iter()
            .map(|(basename, pid, started, u)| OtherProcess {
                basename,
                identity: Identity { boot: self.b.clone().unwrap_or_else(|| format!("session:{}", self.s)), pid, started },
                usage: unpack(u),
            })
            .collect();
        AttributionSample {
            resolution: if self.q { "minute_peak" } else { "sample" }.into(),
            observations: self.k,
            wall_ms: self.w,
            monotonic_ms: self.m,
            session: self.s,
            boot: self.b,
            interval_ms: self.i,
            collection_ms: self.c,
            coverage,
            nitely: unpack(self.n),
            related: unpack(self.r),
            other: unpack(self.o),
            unattributed_cpu_percent: self.u,
            workloads,
            top_other,
        }
    }
}

fn bounded_json(sample: &mut AttributionSample) -> Result<String> {
    let mut json = serde_json::to_string(&Compact::from(sample))?;
    while json.len() > 1792 && (!sample.top_other.is_empty() || !sample.workloads.is_empty()) {
        sample.coverage.truncated = true;
        if sample.top_other.len() > 2 || sample.workloads.is_empty() {
            sample.top_other.pop();
        } else {
            sample.workloads.pop();
        }
        json = serde_json::to_string(&Compact::from(sample))?;
    }
    Ok(json)
}
impl Usage {
    fn peaks(&mut self, other: &Self) {
        fn peak(a: &mut Option<u64>, b: Option<u64>) {
            if let Some(b) = b {
                *a = Some(a.unwrap_or(0).max(b));
            }
        }
        if let Some(cpu) = other.cpu_percent {
            self.cpu_percent = Some(self.cpu_percent.unwrap_or(0.).max(cpu));
        }
        peak(&mut self.private_commit_bytes, other.private_commit_bytes);
        peak(&mut self.working_set_sum_bytes, other.working_set_sum_bytes);
        if let Some(v) = other.process_read_bytes {
            self.process_read_bytes = Some(self.process_read_bytes.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = other.process_write_bytes {
            self.process_write_bytes = Some(self.process_write_bytes.unwrap_or(0).saturating_add(v));
        }
        self.processes = self.processes.max(other.processes);
    }
}
impl AttributionSample {
    fn merge_minute(&mut self, next: &Self) {
        self.wall_ms = next.wall_ms;
        self.monotonic_ms = next.monotonic_ms;
        self.interval_ms = Some(self.interval_ms.unwrap_or(0) + next.interval_ms.unwrap_or(0));
        self.collection_ms = self.collection_ms.max(next.collection_ms);
        self.observations += 1;
        self.nitely.peaks(&next.nitely);
        self.other.peaks(&next.other);
        self.related.peaks(&next.related);
        self.unattributed_cpu_percent = self.unattributed_cpu_percent.into_iter().chain(next.unattributed_cpu_percent).reduce(f64::max);
        self.coverage.enumerated = self.coverage.enumerated.max(next.coverage.enumerated);
        self.coverage.measured = self.coverage.measured.max(next.coverage.measured);
        self.coverage.denied = self.coverage.denied.max(next.coverage.denied);
        self.coverage.missing_identity = self.coverage.missing_identity.max(next.coverage.missing_identity);
        self.coverage.partial_metrics = self.coverage.partial_metrics.max(next.coverage.partial_metrics);
        self.coverage.truncated |= next.coverage.truncated;
        self.coverage.first_sample |= next.coverage.first_sample;
        for w in &next.workloads {
            if let Some(old) = self.workloads.iter_mut().find(|old| old.id == w.id) {
                old.usage.peaks(&w.usage);
            } else {
                self.workloads.push(w.clone());
            }
        }
        for p in &next.top_other {
            if let Some(old) = self.top_other.iter_mut().find(|old| old.identity == p.identity) {
                old.usage.peaks(&p.usage);
            } else {
                self.top_other.push(p.clone());
            }
        }
        let rank = |u: &Usage| u.cpu_percent.unwrap_or(0.) * 1e9 + u.private_commit_bytes.unwrap_or(0) as f64;
        self.workloads.sort_by(|a, b| rank(&b.usage).total_cmp(&rank(&a.usage)));
        self.top_other.sort_by(|a, b| rank(&b.usage).total_cmp(&rank(&a.usage)));
        if self.workloads.len() > 12 || self.top_other.len() > 5 {
            self.coverage.truncated = true;
        }
        self.workloads.truncate(12);
        self.top_other.truncate(5);
    }
}

pub(crate) fn schema(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS workloads(id TEXT PRIMARY KEY,json TEXT NOT NULL CHECK(length(json)<=2048),last_seq INTEGER NOT NULL DEFAULT 0,ended INTEGER);
      CREATE TABLE IF NOT EXISTS attribution(id INTEGER PRIMARY KEY AUTOINCREMENT,wall_ms INTEGER NOT NULL,session TEXT NOT NULL,monotonic_ms INTEGER NOT NULL,json TEXT NOT NULL CHECK(length(json)<=1792));
      CREATE INDEX IF NOT EXISTS attribution_time ON attribution(wall_ms);
      CREATE TABLE IF NOT EXISTS attribution_minutes(id INTEGER PRIMARY KEY AUTOINCREMENT,seq INTEGER NOT NULL,wall_ms INTEGER NOT NULL,json TEXT NOT NULL CHECK(length(json)<=1792));
      CREATE INDEX IF NOT EXISTS attribution_minutes_time ON attribution_minutes(wall_ms);
      CREATE INDEX IF NOT EXISTS attribution_minutes_seq ON attribution_minutes(seq);
      CREATE INDEX IF NOT EXISTS workloads_ended ON workloads(ended);")?;
    Ok(())
}
impl Store {
    fn save_observed_exits(&self, ended: &[(String, String)], wall_ms: i64) -> Result<()> {
        if ended.is_empty() {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        for (id, json) in ended {
            // Roots were read before this transaction. A launcher may have finished since then;
            // its actual end time and Job Object accounting take precedence over our inference.
            tx.execute("UPDATE workloads SET json=?2,ended=?3 WHERE id=?1 AND ended IS NULL", params![id, json, wall_ms])?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Newest insertion first. Project filters only workload rows, never machine-wide totals.
    /// At most 1,000 samples; reads neither collect nor initialize telemetry.
    pub fn attribution(&self, from: i64, to: i64, limit: usize, project: Option<&str>) -> Result<Vec<AttributionSample>> {
        self.attribution_for_boot(from, to, limit, project, None)
    }
    pub(crate) fn attribution_for_boot(&self, from: i64, to: i64, limit: usize, project: Option<&str>, boot: Option<&str>) -> Result<Vec<AttributionSample>> {
        if from > to || limit == 0 || limit > 1000 {
            bail!("invalid attribution range or limit (1–1000)");
        }
        let exists: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='attribution')", [], |r| r.get(0))?;
        if !exists {
            return Ok(vec![]);
        }
        let mut stmt = self.db.prepare("SELECT json FROM (
            SELECT * FROM (SELECT id AS seq,json FROM attribution WHERE wall_ms>=?1 AND wall_ms<=?2 AND (?4 IS NULL OR CASE WHEN json_valid(json) THEN json_extract(json,'$.b') END=?4) ORDER BY id DESC LIMIT ?3)
            UNION ALL SELECT * FROM (SELECT seq,json FROM attribution_minutes WHERE seq<COALESCE((SELECT MIN(id) FROM attribution),9223372036854775807) AND wall_ms>=?1 AND wall_ms<=?2 AND (?4 IS NULL OR CASE WHEN json_valid(json) THEN json_extract(json,'$.b') END=?4) ORDER BY seq DESC LIMIT ?3)
            ) ORDER BY seq DESC LIMIT ?3")?;
        let rows = stmt.query_map(params![from, to, limit as i64, boot], |r| r.get::<_, String>(0))?;
        let mut compact = Vec::new();
        for json in rows {
            if let Ok(row) = serde_json::from_str::<Compact>(&json?) {
                compact.push(row);
            }
        }
        let ids: Vec<_> = compact.iter().flat_map(|row| row.j.iter().map(|(id, _)| id.clone())).collect::<HashSet<_>>().into_iter().collect();
        let mut roots = HashMap::new();
        for ids in ids.chunks(1000) {
            for w in self.workloads(ids)? {
                roots.insert(w.id.clone(), w);
            }
        }
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for compact in compact {
            let mut sample = compact.expand(&roots);
            if !seen.insert((sample.session.clone(), sample.monotonic_ms)) {
                continue;
            }
            if let Some(p) = project {
                sample.workloads.retain(|w| w.context.project.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(p)));
            }
            result.push(sample);
        }
        Ok(result)
    }
    /// Oldest/newest retained wall time, including compacted minute history.
    pub fn attribution_bounds(&self) -> Result<Option<(i64, i64)>> {
        self.attribution_bounds_for_boot(None)
    }
    pub(crate) fn attribution_bounds_for_boot(&self, boot: Option<&str>) -> Result<Option<(i64, i64)>> {
        let exists: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='attribution')", [], |r| r.get(0))?;
        if !exists {
            return Ok(None);
        }
        let (first,last):(Option<i64>,Option<i64>)=self.db.query_row("SELECT MIN(first),MAX(last) FROM (SELECT MIN(wall_ms) AS first,MAX(wall_ms) AS last FROM attribution WHERE ?1 IS NULL OR CASE WHEN json_valid(json) THEN json_extract(json,'$.b') END=?1 UNION ALL SELECT MIN(wall_ms),MAX(wall_ms) FROM attribution_minutes WHERE ?1 IS NULL OR CASE WHEN json_valid(json) THEN json_extract(json,'$.b') END=?1)",[boot],|r|Ok((r.get(0)?,r.get(1)?)))?;
        Ok(first.zip(last))
    }
    pub fn attribution_latest(&self, project: Option<&str>) -> Result<Option<AttributionSample>> {
        Ok(self.attribution(i64::MIN, i64::MAX, 1, project)?.into_iter().next())
    }
    pub fn workloads(&self, ids: &[String]) -> Result<Vec<Workload>> {
        if ids.len() > 1000 {
            bail!("at most 1000 workload identities per query");
        }
        let exists: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workloads')", [], |r| r.get(0))?;
        if !exists {
            return Ok(vec![]);
        }
        let mut query = self.db.prepare("SELECT json FROM workloads WHERE id=?1")?;
        let mut result = Vec::new();
        for id in ids {
            if let Some(json) = query.query_row([id], |r| r.get::<_, String>(0)).optional()? {
                if let Ok(w) = serde_json::from_str(&json) {
                    result.push(w);
                }
            }
        }
        Ok(result)
    }
    /// Includes registered work that exited entirely between samples, with its separate Job totals.
    pub fn recent_workloads(&self, from: i64, to: i64, limit: usize, project: Option<&str>) -> Result<Vec<Workload>> {
        self.recent_workloads_for_boot(from, to, limit, project, None)
    }
    pub(crate) fn recent_workloads_for_boot(&self, from: i64, to: i64, limit: usize, project: Option<&str>, boot: Option<&str>) -> Result<Vec<Workload>> {
        if from > to || limit == 0 || limit > 1000 {
            bail!("invalid workload range or limit (1–1000)");
        }
        let exists: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workloads')", [], |r| r.get(0))?;
        if !exists {
            return Ok(vec![]);
        }
        let mut stmt = self.db.prepare("SELECT json FROM workloads WHERE ended IS NULL OR ended>=?1 ORDER BY rowid DESC LIMIT 8192")?;
        let rows = stmt.query_map([from], |r| r.get::<_, String>(0))?;
        let mut result = Vec::new();
        for json in rows {
            if let Ok(w) = serde_json::from_str::<Workload>(&json?) {
                if w.start_ms <= to && boot.is_none_or(|b| w.root.boot == b) && project.is_none_or(|p| w.context.project.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(p))) {
                    result.push(w);
                    if result.len() == limit {
                        break;
                    }
                }
            }
        }
        Ok(result)
    }
    fn sampling_roots(&self, owners: &HashMap<Identity, String>) -> Result<Vec<Workload>> {
        let mut stmt = self.db.prepare("SELECT json FROM workloads WHERE ended IS NULL ORDER BY rowid DESC LIMIT 8192")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut roots: Vec<Workload> = rows.filter_map(|r| r.ok().and_then(|json| serde_json::from_str(&json).ok())).collect();
        let active: HashSet<_> = roots.iter().map(|w| w.id.clone()).collect();
        let past: Vec<_> = owners.values().filter(|id| !active.contains(*id)).cloned().collect::<HashSet<_>>().into_iter().collect();
        for ids in past.chunks(1000) {
            roots.extend(self.workloads(ids)?);
        }
        Ok(roots)
    }
    pub(crate) fn append_attribution(&mut self, sample: &mut AttributionSample, minute: bool) -> Result<()> {
        let json = bounded_json(sample)?;
        let settings = self.settings()?;
        let previous: Option<String> =
            if minute { None } else { self.db.query_row("SELECT json FROM attribution_minutes ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).optional()? };
        let mut summary = if let Some(previous) = previous.and_then(|s| serde_json::from_str::<Compact>(&s).ok()) {
            let ids: Vec<_> = previous.j.iter().map(|(id, _)| id.clone()).collect();
            let roots = self.workloads(&ids)?.into_iter().map(|w| (w.id.clone(), w)).collect();
            let mut previous = previous.expand(&roots);
            previous.merge_minute(sample);
            previous
        } else {
            let mut summary = sample.clone();
            summary.resolution = "minute_peak".into();
            summary
        };
        let summary_json = bounded_json(&mut summary)?;
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM attribution WHERE wall_ms<?1", [sample.wall_ms.saturating_sub(i64::from(settings.detailed_hours) * 3600000)])?;
        tx.execute("DELETE FROM attribution WHERE id <= (SELECT MAX(id) FROM attribution)-?1", [i64::from(settings.detailed_hours) * 720 - 1])?;
        tx.execute(
            "INSERT INTO attribution(wall_ms,session,monotonic_ms,json) VALUES(?1,?2,?3,?4)",
            params![sample.wall_ms, sample.session, sample.monotonic_ms.min(i64::MAX as u64) as i64, json],
        )?;
        let seq = tx.last_insert_rowid();
        for id in summary.workloads.iter().chain(&sample.workloads).map(|w| &w.id).collect::<HashSet<_>>() {
            tx.execute("UPDATE workloads SET last_seq=?2 WHERE id=?1", params![id, seq])?;
        }
        if minute {
            tx.execute("DELETE FROM attribution_minutes WHERE wall_ms<?1", [sample.wall_ms.saturating_sub(i64::from(settings.summary_days) * 86400000)])?;
            tx.execute("DELETE FROM attribution_minutes WHERE id <= (SELECT MAX(id) FROM attribution_minutes)-?1", [i64::from(settings.summary_days) * 1440 - 1])?;
            tx.execute("INSERT INTO attribution_minutes(seq,wall_ms,json) VALUES(?1,?2,?3)", params![seq, sample.wall_ms, summary_json])?;
        } else {
            tx.execute("UPDATE attribution_minutes SET seq=?1,wall_ms=?2,json=?3 WHERE id=(SELECT MAX(id) FROM attribution_minutes)", params![seq, sample.wall_ms, summary_json])?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// One thread's explicitly supplied launch context. In-memory test DBs never use the real folder.
#[derive(Clone)]
struct LaunchContext {
    dir: PathBuf,
    context: Context,
}
thread_local! { static LAUNCH: std::cell::RefCell<Option<LaunchContext>> = const { std::cell::RefCell::new(None) }; }
pub struct Scope(Option<LaunchContext>);
impl Scope {
    pub fn new(db: &crate::Db, mut context: Context) -> Self {
        LAUNCH.with(|s| {
            if let Some(parent) = s.borrow().as_ref().filter(|p| p.context.issue == context.issue) {
                context.job_id = context.job_id.or(parent.context.job_id);
                context.worker_id = context.worker_id.or(parent.context.worker_id);
            }
        });
        let dir = db.conn.path().filter(|p| !p.is_empty()).and_then(|p| Path::new(p).parent()).map(|p| p.join("performance"));
        let next = dir.filter(|d| d.join("history.sqlite").exists()).map(|dir| LaunchContext { dir, context });
        Self(LAUNCH.with(|s| s.replace(next)))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        LAUNCH.with(|s| s.replace(self.0.take()));
    }
}

pub struct Tracked {
    dir: PathBuf,
    workload: Workload,
}
impl Tracked {
    pub fn finish(mut self, accounting: Option<crate::procs::Accounting>) {
        self.workload.end_ms = Some(crate::performance::wall_ms());
        self.workload.job_accounting = accounting;
        let _ = save_workload_inner(&self.dir, &self.workload, false);
    }
}
fn save_workload(dir: &Path, workload: &Workload) -> Result<()> {
    save_workload_inner(dir, workload, true)
}
fn save_workload_inner(dir: &Path, workload: &Workload, create: bool) -> Result<()> {
    // Registration never initializes a store or waits on recorder ownership; launch must proceed.
    let mut db = rusqlite::Connection::open_with_flags(dir.join("history.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_millis(20))?;
    db.pragma_update(None, "max_page_count", 65536)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("SELECT version FROM recorder_version WHERE id=1", [], |r| r.get(0))?;
    if version != crate::performance::RECORDER_VERSION {
        return Ok(());
    }
    let settings: Option<String> = tx.query_row("SELECT json FROM settings WHERE id=1", [], |r| r.get(0)).optional()?;
    let settings = settings.map(|s| serde_json::from_str::<crate::performance::Settings>(&s)).transpose()?.unwrap_or_default();
    if !settings.enabled {
        return Ok(());
    }
    let json = serde_json::to_string(workload)?;
    if json.len() > 2048 {
        bail!("workload metadata exceeds its bound");
    }
    tx.execute(
        "DELETE FROM workloads WHERE ended<?1 AND last_seq < MIN(COALESCE((SELECT MIN(id) FROM attribution),9223372036854775807),COALESCE((SELECT MIN(seq) FROM attribution_minutes),9223372036854775807))",
        [crate::performance::wall_ms().saturating_sub(i64::from(settings.summary_days) * 86400000)],
    )?;
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workloads WHERE id=?1)", [&workload.id], |r| r.get(0))?;
    if !create && !exists {
        return Ok(());
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM workloads", [], |r| r.get(0))?;
    if !exists && count >= 8192 {
        bail!("workload metadata capacity reached");
    }
    tx.execute(
        "INSERT INTO workloads(id,json,ended) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET json=excluded.json,ended=excluded.ended",
        params![workload.id, json, workload.end_ms],
    )?;
    tx.commit()?;
    Ok(())
}
/// Each live client checks only its own dictionary key after clear/resume. This never restores
/// old samples or completed metadata, and a paused/newer recorder rejects the write atomically.
pub(crate) fn ensure_service(dir: &Path) {
    let Some(boot) = crate::performance::boot_id() else {
        return;
    };
    let pid = std::process::id();
    let Some(started) = crate::procs::started(pid) else {
        return;
    };
    let id = format!("{boot}:{pid}:{started}:service");
    let exists = (|| -> Result<bool> {
        let db = rusqlite::Connection::open_with_flags(dir.join("history.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(std::time::Duration::from_millis(20))?;
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM workloads WHERE id=?1)", [&id], |r| r.get(0))?)
    })();
    if matches!(exists, Ok(false)) {
        register_started(dir, pid, started, "service", Context::default());
    }
}
pub fn track(pid: u32, kind: &str) -> Option<Tracked> {
    let launch = LAUNCH.with(|s| s.borrow().clone())?;
    register(&launch.dir, pid, kind, launch.context)
}
pub fn track_child(child: &std::process::Child, kind: &str) -> Option<Tracked> {
    let launch = LAUNCH.with(|s| s.borrow().clone())?;
    register_started(&launch.dir, child.id(), crate::procs::child_started(child)?, kind, launch.context)
}
pub(crate) fn register(dir: &Path, pid: u32, kind: &str, context: Context) -> Option<Tracked> {
    let started = crate::procs::started(pid)?;
    register_started(dir, pid, started, kind, context)
}
fn register_started(dir: &Path, pid: u32, started: String, kind: &str, context: Context) -> Option<Tracked> {
    let boot = crate::performance::boot_id()?;
    let kind = match kind {
        "app" | "service" | "worker" | "review" | "build" | "gate" | "land" | "setup" | "submit" | "install" => kind,
        _ => return None,
    };
    let root = Identity { boot, pid, started };
    let workload = Workload {
        id: format!("{}:{}:{}:{kind}", root.boot, pid, root.started),
        kind: kind.into(),
        context,
        root,
        start_ms: crate::performance::wall_ms(),
        end_ms: None,
        end_observed: false,
        job_accounting: None,
    };
    save_workload(dir, &workload).ok()?;
    Some(Tracked { dir: dir.into(), workload })
}

#[derive(Default)]
pub(crate) struct Sampler {
    previous: HashMap<Identity, crate::procs::ProcessMetrics>,
    owners: HashMap<Identity, String>,
    last: Option<(String, u64)>,
    exit_cursor: usize,
}
impl Sampler {
    pub(crate) fn collect(&mut self, store: &Store, machine: &Sample) -> Result<AttributionSample> {
        let at = std::time::Instant::now();
        let snapshot = crate::procs::metrics();
        let mut roots = store.sampling_roots(&self.owners)?;
        if snapshot.supported {
            let live: HashMap<_, _> = snapshot.processes.iter().map(|p| (p.pid, p.started.as_str())).collect();
            let count = roots.len();
            let mut ended = Vec::new();
            for offset in 0..count.min(64) {
                let root = &mut roots[(self.exit_cursor + offset) % count];
                if root.end_ms.is_some() {
                    continue;
                }
                let gone = machine.boot.as_ref().is_some_and(|boot| boot != &root.root.boot)
                    || live.get(&root.root.pid).is_some_and(|start| *start != root.root.started)
                    || (!live.contains_key(&root.root.pid) && !crate::procs::alive(root.root.pid));
                if gone {
                    root.end_ms = Some(machine.wall_ms);
                    root.end_observed = true;
                    ended.push((root.id.clone(), serde_json::to_string(root)?));
                }
            }
            store.save_observed_exits(&ended, machine.wall_ms)?;
        }
        if !roots.is_empty() {
            self.exit_cursor = (self.exit_cursor + 64) % roots.len();
        }
        let mut sample = self.attribute(machine, snapshot, &roots);
        let metadata_count: i64 = store.db.query_row("SELECT COUNT(*) FROM workloads", [], |r| r.get(0))?;
        sample.coverage.truncated |= metadata_count >= 8192;
        sample.collection_ms = at.elapsed().as_millis() as u64;
        Ok(sample)
    }
    fn attribute(&mut self, machine: &Sample, snapshot: crate::procs::ProcessSnapshot, roots: &[Workload]) -> AttributionSample {
        let interval = self.last.as_ref().filter(|(s, t)| s == &machine.session && *t < machine.monotonic_ms).map(|(_, t)| machine.monotonic_ms - t);
        let boot = machine.boot.clone().unwrap_or_else(|| format!("session:{}", machine.session));
        let mut coverage = Coverage {
            enumerated: snapshot.processes.len() as u32 + snapshot.denied + snapshot.missing_identity,
            denied: snapshot.denied,
            missing_identity: snapshot.missing_identity,
            truncated: snapshot.truncated || roots.len() >= 8192,
            first_sample: interval.is_none(),
            supported: snapshot.supported,
            short_lived_partial: true,
            ..Coverage::default()
        };
        let identities: HashMap<u32, Identity> = snapshot.processes.iter().map(|p| (p.pid, Identity { boot: boot.clone(), pid: p.pid, started: p.started.clone() })).collect();
        let parents: HashMap<u32, u32> = snapshot.processes.iter().map(|p| (p.pid, p.parent_pid)).collect();
        let by_id: HashMap<&str, &Workload> = roots.iter().map(|w| (w.id.as_str(), w)).collect();
        let mut registered: HashMap<Identity, &Workload> = HashMap::new();
        // More specific roots replace generic app/service registrations of the same process.
        for root in roots.iter().filter(|w| w.end_ms.is_none()) {
            if !matches!(root.kind.as_str(), "app" | "service") || !registered.contains_key(&root.root) {
                registered.insert(root.root.clone(), root);
            }
        }
        let mut grouped: HashMap<String, Usage> = HashMap::new();
        let mut nitely = Usage::default();
        let mut other = Usage::default();
        let mut top_other = Vec::new();
        let cores = snapshot.cores.max(1) as f64;
        let mut next = HashMap::new();
        let mut next_owners = HashMap::new();
        for process in snapshot.processes {
            let identity = identities[&process.pid].clone();
            let old = interval.and_then(|_| self.previous.get(&identity));
            let delta = |now: Option<u64>, before: Option<u64>| now.zip(before).and_then(|(n, b)| n.checked_sub(b));
            let usage = Usage {
                cpu_percent: delta(process.cpu_100ns, old.and_then(|p| p.cpu_100ns)).zip(interval).map(|(cpu, ms)| cpu as f64 / (ms as f64 * 100.) / cores),
                private_commit_bytes: process.private_commit_bytes,
                working_set_sum_bytes: process.working_set_bytes,
                process_read_bytes: delta(process.read_bytes, old.and_then(|p| p.read_bytes)),
                process_write_bytes: delta(process.write_bytes, old.and_then(|p| p.write_bytes)),
                processes: 1,
            };
            if process.cpu_100ns.is_some() || process.private_commit_bytes.is_some() {
                coverage.measured += 1;
            }
            if process.cpu_100ns.is_none()
                || process.private_commit_bytes.is_none()
                || process.working_set_bytes.is_none()
                || process.read_bytes.is_none()
                || process.write_bytes.is_none()
            {
                coverage.partial_metrics += 1;
            }
            let mut cursor = identity.clone();
            let mut seen = HashSet::new();
            let mut owner = None;
            let mut inherited = None;
            while seen.insert(cursor.pid) {
                if let Some(root) = registered.get(&cursor) {
                    owner = Some(root.id.clone());
                    break;
                }
                // A living ancestor whose exact identity was already owned can pass ownership
                // to a new child even after its registered root exits. Keep searching so an
                // explicit registration still overrides cached ownership.
                if inherited.is_none() {
                    inherited = self.owners.get(&cursor).filter(|id| by_id.get(id.as_str()).is_some_and(|w| w.end_ms.is_none() || w.root != cursor)).cloned();
                }
                let Some(parent) = parents.get(&cursor.pid).and_then(|pid| identities.get(pid)) else {
                    break;
                };
                // A reused parent PID must not retroactively adopt an older child.
                if parent.started.parse::<u64>().ok().zip(cursor.started.parse::<u64>().ok()).is_none_or(|(p, c)| p > c) {
                    break;
                }
                cursor = parent.clone();
            }
            if owner.is_none() {
                owner = inherited;
            }
            if let Some(id) = owner {
                nitely.add(&usage);
                grouped.entry(id.clone()).or_default().add(&usage);
                next_owners.insert(identity.clone(), id);
            } else {
                other.add(&usage);
                top_other.push(OtherProcess { basename: process.basename.clone(), identity: identity.clone(), usage });
            }
            next.insert(identity, process);
        }
        let rank = |u: &Usage| u.cpu_percent.unwrap_or(0.) * 1e9 + u.private_commit_bytes.unwrap_or(0) as f64;
        top_other.sort_by(|a, b| rank(&b.usage).total_cmp(&rank(&a.usage)));
        if top_other.len() > 5 {
            coverage.truncated = true;
            top_other.truncate(5);
        }
        let mut workloads: Vec<_> = grouped
            .into_iter()
            .filter_map(|(id, usage)| by_id.get(id.as_str()).map(|w| WorkloadUsage { id: id.clone(), context: w.context.clone(), kind: w.kind.clone(), usage }))
            .collect();
        workloads.sort_by(|a, b| rank(&b.usage).total_cmp(&rank(&a.usage)));
        if workloads.len() > 12 {
            coverage.truncated = true;
            workloads.truncate(12);
        }
        let unattributed_cpu_percent = machine.metrics.cpu_percent.zip(nitely.cpu_percent.zip(other.cpu_percent).map(|(a, b)| a + b)).map(|(m, p)| (m - p).max(0.));
        self.previous = next;
        self.owners = next_owners;
        self.last = Some((machine.session.clone(), machine.monotonic_ms));
        AttributionSample {
            resolution: "sample".into(),
            observations: 1,
            wall_ms: machine.wall_ms,
            monotonic_ms: machine.monotonic_ms,
            session: machine.session.clone(),
            boot: machine.boot.clone(),
            interval_ms: interval,
            collection_ms: 0,
            coverage,
            nitely,
            related: Usage::default(),
            other,
            unattributed_cpu_percent,
            workloads,
            top_other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{ProcessMetrics, ProcessSnapshot};
    fn machine(ms: u64) -> Sample {
        Sample {
            wall_ms: ms as i64,
            monotonic_ms: ms,
            boot: Some("boot".into()),
            session: "session".into(),
            metrics: crate::performance::Metrics { cpu_percent: Some(50.), ..Default::default() },
            collection_ms: 0,
        }
    }
    fn process(pid: u32, parent: u32, start: u64, cpu: u64) -> ProcessMetrics {
        ProcessMetrics {
            pid,
            parent_pid: parent,
            started: start.to_string(),
            basename: "node.exe".into(),
            cpu_100ns: Some(cpu),
            private_commit_bytes: Some(100),
            working_set_bytes: Some(200),
            read_bytes: Some(cpu),
            write_bytes: Some(cpu),
        }
    }
    fn snapshot(p: Vec<ProcessMetrics>) -> ProcessSnapshot {
        ProcessSnapshot { processes: p, supported: true, cores: 2, ..Default::default() }
    }
    fn root(pid: u32, start: u64, kind: &str, project: &str) -> Workload {
        Workload {
            id: format!("work-{pid}-{start}"),
            kind: kind.into(),
            context: Context { project: Some(project.into()), issue: Some(format!("{project}-1")), ..Default::default() },
            root: Identity { boot: "boot".into(), pid, started: start.to_string() },
            start_ms: 1,
            end_ms: None,
            end_observed: false,
            job_accounting: None,
        }
    }

    #[test]
    fn attribution_pid_reuse_and_unregistered_tool_names_do_not_inherit_ownership() {
        let mut s = Sampler::default();
        let roots = [root(1, 10, "worker", "APP")];
        let first = s.attribute(&machine(0), snapshot(vec![process(1, 0, 10, 0)]), &roots);
        assert!(first.nitely.cpu_percent.is_none());
        let second = s.attribute(&machine(1000), snapshot(vec![process(1, 0, 20, 10_000_000), process(8, 0, 1, 30)]), &roots);
        assert_eq!(second.nitely.processes, 0);
        assert_eq!(second.other.processes, 2);
        assert!(second.other.cpu_percent.is_none(), "reused PID cannot produce a CPU delta");
        let mut reboot = machine(2000);
        reboot.boot = Some("other-boot".into());
        assert_eq!(s.attribute(&reboot, snapshot(vec![process(1, 0, 10, 0)]), &roots).nitely.processes, 0);
    }

    #[test]
    fn attribution_keeps_children_after_parent_exit_but_not_reused_parents() {
        let mut s = Sampler::default();
        let mut roots = [root(1, 10, "worker", "APP")];
        s.attribute(&machine(0), snapshot(vec![process(1, 0, 10, 0), process(2, 1, 20, 0)]), &roots);
        roots[0].end_ms = Some(500);
        let next = s.attribute(&machine(1000), snapshot(vec![process(2, 1, 20, 10_000_000)]), &roots);
        assert_eq!(next.nitely.processes, 1);
        assert_eq!(next.nitely.cpu_percent, Some(50.));
        let mut clean = Sampler::default();
        let wrong = [root(1, 30, "worker", "APP")];
        let next = clean.attribute(&machine(0), snapshot(vec![process(1, 0, 30, 0), process(2, 1, 20, 0)]), &wrong);
        assert_eq!(next.nitely.processes, 1);
        assert_eq!(next.other.processes, 1);
    }

    #[test]
    fn attribution_new_descendants_inherit_verified_surviving_ancestors() {
        let mut s = Sampler::default();
        let mut roots = vec![root(1, 10, "worker", "APP")];
        s.attribute(&machine(0), snapshot(vec![process(1, 0, 10, 0), process(2, 1, 20, 0)]), &roots);
        roots[0].end_ms = Some(500);
        // New descendants appear before their parents in the snapshot, so propagation cannot
        // depend on enumeration order or on already processing the parent's current sample.
        let next = s.attribute(&machine(1000), snapshot(vec![process(4, 3, 40, 0), process(3, 2, 30, 0), process(2, 1, 20, 1)]), &roots);
        assert_eq!((next.nitely.processes, next.other.processes), (3, 0));
        assert_eq!(next.workloads[0].id, roots[0].id);
        // An explicit new root supersedes the cached ancestor's old ownership for its descendants.
        roots.push(root(2, 20, "build", "DEMO"));
        let next = s.attribute(&machine(2000), snapshot(vec![process(4, 3, 40, 1), process(3, 2, 30, 1), process(2, 1, 20, 2)]), &roots);
        assert_eq!(next.workloads.len(), 1);
        assert_eq!(next.workloads[0].id, roots[1].id);
        // Reusing the surviving ancestor's PID cannot pass its previous ownership to new children.
        let next = s.attribute(&machine(3000), snapshot(vec![process(5, 2, 60, 0), process(2, 0, 50, 0)]), &roots);
        assert_eq!((next.nitely.processes, next.other.processes), (0, 2));
    }

    #[test]
    fn attribution_nested_and_detached_lanes_count_once_across_projects() {
        let mut s = Sampler::default();
        let roots = vec![root(1, 10, "worker", "APP"), root(2, 20, "review", "APP"), root(3, 30, "build", "DEMO")];
        let processes = vec![process(1, 0, 10, 0), process(2, 1, 20, 0), process(3, 0, 30, 0), process(4, 3, 40, 0), process(5, 2, 50, 0), process(6, 0, 60, 0)];
        let result = s.attribute(&machine(0), snapshot(processes), &roots);
        assert_eq!(result.nitely.processes, 5);
        assert_eq!(result.other.processes, 1);
        assert_eq!(result.workloads.iter().map(|w| w.usage.processes).sum::<u32>(), 5);
        assert_eq!(result.workloads.iter().find(|w| w.kind == "build").unwrap().usage.processes, 2);
        // An explicit nested registration arriving after the first sample supersedes cached ownership.
        let mut s = Sampler::default();
        s.attribute(&machine(0), snapshot(vec![process(1, 0, 10, 0), process(2, 1, 20, 0), process(5, 2, 50, 0)]), &roots[..1]);
        let result = s.attribute(&machine(1000), snapshot(vec![process(1, 0, 10, 0), process(2, 1, 20, 1), process(5, 2, 50, 1)]), &roots);
        assert_eq!(result.workloads.iter().find(|w| w.kind == "review").unwrap().usage.processes, 2);
    }

    #[test]
    fn attribution_partial_permission_and_exit_coverage_are_explicit() {
        let mut p = process(1, 0, 10, 10);
        p.private_commit_bytes = None;
        let result =
            Sampler::default().attribute(&machine(0), ProcessSnapshot { processes: vec![p], denied: 3, missing_identity: 2, supported: true, cores: 2, truncated: true }, &[]);
        assert_eq!(result.coverage.enumerated, 6);
        assert_eq!(result.coverage.denied, 3);
        assert_eq!(result.coverage.missing_identity, 2);
        assert_eq!(result.coverage.partial_metrics, 1);
        assert!(result.coverage.short_lived_partial && result.coverage.truncated);
        assert!(result.other.private_commit_bytes.is_none());
        let result = Sampler::default().attribute(&machine(0), ProcessSnapshot::default(), &[]);
        assert!(!result.coverage.supported);
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(crate::paths::unique_temp("nitely-attribution", ""))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(windows)]
    #[test]
    fn attribution_standalone_gate_registers_issue_and_job_accounting() {
        let dir = Temp::new();
        let repo = dir.0.join("repo");
        crate::test_repo::init(&repo, &[(crate::playbook::PLAYBOOK_PATH, "[gate]\ncommands = [\"echo standalone gate\"]\n")]);
        let mut db = crate::Db::open(&dir.0.join("nitely.db")).unwrap();
        db.create_project("APP", "App").unwrap();
        let actor = crate::Actor::Agent("codex:test".into());
        let issue = db.create_issue(&crate::issues::NewIssue { project: "APP".into(), title: "Gate fixture".into(), kind: "feature".into(), body: "Verify standalone gate workload attribution.".into(), evidence: "Controlled gate fixture.".into(), source: "human".into(), ..Default::default() }, &actor).unwrap();
        let store = Store::open(&dir.0.join("performance")).unwrap();
        assert!(LAUNCH.with(|launch| launch.borrow().is_none()), "direct gate begins without a caller scope");
        let result = crate::playbook::run_gate(&db, &repo, &repo, Some(&issue.key), &actor).unwrap();
        assert!(result.passed);
        let work = store.recent_workloads(0, i64::MAX, 10, None).unwrap();
        assert_eq!(work.len(), 1, "standalone command must register independently of the generic service");
        assert_eq!(work[0].context.project.as_deref(), Some("APP"));
        assert_eq!(work[0].context.issue.as_deref(), Some(issue.key.as_str()));
        assert!(work[0].end_ms.is_some() && !work[0].end_observed);
        assert!(work[0].job_accounting.as_ref().unwrap().processes >= 1);
        assert!(LAUNCH.with(|launch| launch.borrow().is_none()), "gate restores its caller context");
    }
    #[test]
    fn attribution_history_keeps_context_after_exit_filters_only_workloads_and_is_read_only() {
        let dir = Temp::new();
        let mut store = Store::open(&dir.0).unwrap();
        let roots = [root(1, 10, "worker", "APP"), root(2, 20, "review", "DEMO")];
        for r in &roots {
            save_workload(&dir.0, r).unwrap();
        }
        let mut sample = Sampler::default().attribute(&machine(5000), snapshot(vec![process(1, 0, 10, 0), process(2, 0, 20, 0), process(3, 0, 30, 0)]), &roots);
        store.append_attribution(&mut sample, true).unwrap();
        let mut ended = roots[0].clone();
        ended.end_ms = Some(6000);
        save_workload(&dir.0, &ended).unwrap();
        drop(store);
        let read = Store::read(&dir.0).unwrap().unwrap();
        let latest = read.attribution_latest(Some("APP")).unwrap().unwrap();
        assert_eq!(latest.workloads.len(), 1);
        assert_eq!(latest.nitely.processes, 2);
        assert_eq!(latest.other.processes, 1);
        assert_eq!(read.workloads(std::slice::from_ref(&roots[0].id)).unwrap()[0].end_ms, Some(6000));
        assert_eq!(read.recent_workloads(0, 7000, 100, Some("APP")).unwrap().len(), 1);
        assert!(read.db.execute("DELETE FROM attribution", []).is_err());
        assert!(read.attribution(0, 10, 1001, None).is_err());
        assert_eq!(read.attribution_bounds().unwrap(), Some((5000, 5000)));
    }

    #[test]
    fn attribution_explicit_finish_wins_over_stale_observed_exit() {
        let dir = Temp::new();
        let store = Store::open(&dir.0).unwrap();
        for observe_first in [false, true] {
            let workload = root(if observe_first { 2 } else { 1 }, 10, "build", "APP");
            save_workload(&dir.0, &workload).unwrap();
            let mut cached = store.sampling_roots(&HashMap::new()).unwrap().into_iter().find(|w| w.id == workload.id).unwrap();
            let observed_at = i64::MAX;
            cached.end_ms = Some(observed_at);
            cached.end_observed = true;
            let stale = vec![(cached.id.clone(), serde_json::to_string(&cached).unwrap())];
            if observe_first {
                store.save_observed_exits(&stale, observed_at).unwrap();
                assert!(store.workloads(std::slice::from_ref(&workload.id)).unwrap()[0].end_observed);
            }
            // Tracked::finish writes through a second connection after the sampler read its roots.
            let accounting = crate::procs::Accounting { cpu_100ns: 123, read_bytes: 456, write_bytes: 789, processes: 3, peak_private_commit_bytes: Some(1024) };
            Tracked { dir: dir.0.clone(), workload: workload.clone() }.finish(Some(accounting));
            let finished = store.workloads(std::slice::from_ref(&workload.id)).unwrap().remove(0);
            assert!(finished.end_ms.is_some_and(|at| at < observed_at));
            assert!(!finished.end_observed);
            assert_eq!(finished.job_accounting.as_ref().unwrap().write_bytes, 789);
            store.save_observed_exits(&stale, observed_at).unwrap();
            let persisted = store.workloads(std::slice::from_ref(&workload.id)).unwrap().remove(0);
            assert_eq!(serde_json::to_value(&persisted).unwrap(), serde_json::to_value(&finished).unwrap(), "stale observation must preserve all explicit completion metadata");
        }
    }

    #[test]
    fn attribution_detail_references_remain_even_when_minute_breakdown_is_trimmed() {
        let dir = Temp::new();
        let mut store = Store::open(&dir.0).unwrap();
        let mut roots = Vec::new();
        for pid in 1..=20 {
            let mut w = root(pid, 10, "worker", "APP");
            w.id = format!("work-{pid:02}-{}", "a".repeat(64));
            save_workload(&dir.0, &w).unwrap();
            roots.push(w);
        }
        let mut first = Sampler::default().attribute(&machine(5000), snapshot(vec![]), &[]);
        first.workloads = roots[..10]
            .iter()
            .map(|w| WorkloadUsage {
                id: w.id.clone(),
                context: w.context.clone(),
                kind: w.kind.clone(),
                usage: Usage {
                    cpu_percent: Some(99.),
                    private_commit_bytes: Some(9999999999),
                    working_set_sum_bytes: Some(9999999999),
                    process_read_bytes: Some(9999999999),
                    process_write_bytes: Some(9999999999),
                    processes: 99,
                },
            })
            .collect();
        store.append_attribution(&mut first, true).unwrap();
        let mut later = first.clone();
        later.wall_ms = 10000;
        later.monotonic_ms = 10000;
        later.workloads = roots[10..]
            .iter()
            .map(|w| WorkloadUsage {
                id: w.id.clone(),
                context: w.context.clone(),
                kind: w.kind.clone(),
                usage: Usage { cpu_percent: Some(1.), ..first.workloads[0].usage.clone() },
            })
            .collect();
        store.append_attribution(&mut later, false).unwrap();
        let summary: Compact =
            serde_json::from_str(&store.db.query_row("SELECT json FROM attribution_minutes ORDER BY id DESC LIMIT 1", [], |r| r.get::<_, String>(0)).unwrap()).unwrap();
        assert!(later.workloads.iter().any(|w| !summary.j.iter().any(|(id, _)| id == &w.id)), "fixture must trim a detailed contributor from minute breakdown");
        let seq: i64 = store.db.query_row("SELECT MAX(id) FROM attribution", [], |r| r.get(0)).unwrap();
        for w in later.workloads {
            assert_eq!(store.db.query_row("SELECT last_seq FROM workloads WHERE id=?1", [w.id], |r| r.get::<_, i64>(0)).unwrap(), seq);
        }
    }

    #[test]
    fn attribution_metadata_survives_minute_references_after_detail_expires() {
        let dir = Temp::new();
        let mut store = Store::open(&dir.0).unwrap();
        let mut workload = root(1, 10, "worker", "APP");
        save_workload(&dir.0, &workload).unwrap();
        let mut sample = Sampler::default().attribute(&machine(5000), snapshot(vec![process(1, 0, 10, 0)]), std::slice::from_ref(&workload));
        store.append_attribution(&mut sample, true).unwrap();
        workload.end_ms = Some(crate::performance::wall_ms() - 31 * 86400000);
        save_workload(&dir.0, &workload).unwrap();
        store.db.execute("DELETE FROM attribution", []).unwrap();
        save_workload(&dir.0, &root(2, 20, "build", "APP")).unwrap();
        assert_eq!(store.workloads(std::slice::from_ref(&workload.id)).unwrap().len(), 1);
        assert_eq!(store.attribution_latest(None).unwrap().unwrap().workloads[0].context.project.as_deref(), Some("APP"));
        store.db.execute("DELETE FROM attribution_minutes", []).unwrap();
        save_workload(&dir.0, &root(3, 30, "build", "APP")).unwrap();
        assert!(store.workloads(&[workload.id]).unwrap().is_empty());
    }

    #[test]
    fn attribution_registration_respects_pause_clear_and_newer_generation() {
        let dir = Temp::new();
        let store = Store::open(&dir.0).unwrap();
        let workload = root(1, 10, "worker", "APP");
        save_workload(&dir.0, &workload).unwrap();
        store.configure(&crate::performance::Settings { enabled: false, ..Default::default() }).unwrap();
        crate::performance::clear_history(&dir.0).unwrap();
        save_workload(&dir.0, &workload).unwrap();
        assert!(store.workloads(std::slice::from_ref(&workload.id)).unwrap().is_empty());
        store.configure(&crate::performance::Settings::default()).unwrap();
        Tracked { dir: dir.0.clone(), workload: workload.clone() }.finish(None);
        assert!(store.workloads(std::slice::from_ref(&workload.id)).unwrap().is_empty(), "late finish must not restore cleared metadata");
        store.db.execute("UPDATE recorder_version SET version=version+1", []).unwrap();
        save_workload(&dir.0, &workload).unwrap();
        assert!(store.workloads(&[workload.id]).unwrap().is_empty());
    }
    #[test]
    #[cfg(windows)]
    fn attribution_live_service_registers_again_after_clear_and_resume() {
        let dir = Temp::new();
        let store = Store::open(&dir.0).unwrap();
        ensure_service(&dir.0);
        let first = store.recent_workloads(0, i64::MAX, 10, None).unwrap();
        assert_eq!(first.len(), 1);
        ensure_service(&dir.0);
        assert_eq!(store.recent_workloads(0, i64::MAX, 10, None).unwrap()[0].start_ms, first[0].start_ms);
        store.configure(&crate::performance::Settings { enabled: false, ..Default::default() }).unwrap();
        crate::performance::clear_history(&dir.0).unwrap();
        ensure_service(&dir.0);
        assert!(store.recent_workloads(0, i64::MAX, 10, None).unwrap().is_empty());
        store.configure(&crate::performance::Settings::default()).unwrap();
        ensure_service(&dir.0);
        let resumed = store.recent_workloads(0, i64::MAX, 10, None).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].id, first[0].id);
        assert!(resumed[0].start_ms >= first[0].start_ms);
        assert!(store.attribution_latest(None).unwrap().is_none());
    }

    #[test]
    fn attribution_scope_cannot_fall_back_from_a_test_database_to_the_real_recorder() {
        let memory = crate::Db { conn: rusqlite::Connection::open_in_memory().unwrap() };
        let scope = Scope::new(&memory, Context::default());
        assert!(LAUNCH.with(|s| s.borrow().is_none()));
        drop(scope);
        let dir = Temp::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let db = crate::Db { conn: rusqlite::Connection::open(dir.0.join("test.db")).unwrap() };
        Store::open(&dir.0.join("performance")).unwrap();
        let _scope = Scope::new(&db, Context { issue: Some("APP-1".into()), worker_id: Some(12), ..Default::default() });
        assert_eq!(LAUNCH.with(|s| s.borrow().as_ref().unwrap().dir.clone()), dir.0.join("performance"));
        {
            let _inner = Scope::new(&db, Context { issue: Some("APP-1".into()), run_id: Some(9), ..Default::default() });
            assert_eq!(LAUNCH.with(|s| s.borrow().as_ref().unwrap().context.worker_id), Some(12));
        }
        assert_eq!(LAUNCH.with(|s| s.borrow().as_ref().unwrap().context.run_id), None);
    }

    #[test]
    fn attribution_large_process_fixture_is_bounded_and_never_classifies_by_name() {
        let at = std::time::Instant::now();
        let mut s = Sampler::default();
        let sample = s.attribute(&machine(0), snapshot((1..=10_000).map(|id| process(id, 0, id as u64, 0)).collect()), &[]);
        assert_eq!(sample.other.processes, 10_000);
        assert_eq!(sample.nitely.processes, 0);
        assert_eq!(sample.top_other.len(), 5);
        assert!(sample.coverage.truncated);
        eprintln!("10,000 synthetic process attribution: {:?}", at.elapsed());
        assert!(at.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn attribution_minute_keeps_spikes_and_sums_io_without_summing_memory() {
        let dir = Temp::new();
        let mut store = Store::open(&dir.0).unwrap();
        let mut sampler = Sampler::default();
        sampler.attribute(&machine(0), snapshot(vec![process(1, 0, 10, 0)]), &[]);
        let mut first = sampler.attribute(&machine(5000), snapshot(vec![process(1, 0, 10, 10_000_000)]), &[]);
        store.append_attribution(&mut first, true).unwrap();
        let mut later = sampler.attribute(&machine(10000), snapshot(vec![process(1, 0, 10, 10_000_100)]), &[]);
        store.append_attribution(&mut later, false).unwrap();
        // Drop detailed rows to inspect the durable minute representation.
        store.db.execute("DELETE FROM attribution", []).unwrap();
        let minute = store.attribution_latest(None).unwrap().unwrap();
        assert_eq!(minute.resolution, "minute_peak");
        assert_eq!(minute.observations, 2);
        assert_eq!(minute.other.cpu_percent, Some(10.));
        assert_eq!(minute.other.process_read_bytes, Some(10_000_100));
        assert_eq!(minute.other.private_commit_bytes, Some(100));
        assert_eq!(minute.interval_ms, Some(10000));
    }

    #[test]
    #[cfg(windows)]
    fn attribution_native_fixture_keeps_short_lived_job_accounting() {
        let dir = Temp::new();
        Store::open(&dir.0).unwrap();
        let guard = crate::procs::TreeGuard::new().unwrap();
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "echo controlled fixture"]);
        command.stdout(std::process::Stdio::null());
        let mut child = guard.spawn(&mut command).unwrap();
        child.wait().unwrap();
        let started = crate::procs::child_started(&child).expect("child handle retains identity after exit");
        let tracked = register_started(&dir.0, child.id(), started, "build", Context::default()).unwrap();
        tracked.finish(guard.accounting());
        let store = Store::read(&dir.0).unwrap().unwrap();
        let jobs = store.recent_workloads(0, i64::MAX, 10, None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].end_ms.is_some());
        assert!(jobs[0].job_accounting.as_ref().unwrap().processes >= 1);
        let at = std::time::Instant::now();
        let snapshot = crate::procs::metrics();
        eprintln!("native process snapshot: {:?}, {} processes, {} denied", at.elapsed(), snapshot.processes.len(), snapshot.denied);
        let own = snapshot.processes.iter().find(|p| p.pid == std::process::id()).unwrap();
        assert!(own.private_commit_bytes.is_some() && own.working_set_bytes.is_some() && own.cpu_100ns.is_some() && own.read_bytes.is_some());
        assert!(snapshot.processes.iter().all(|p| !p.basename.contains(['\\', '/'])));
    }

    #[test]
    #[ignore = "bounded full-retention CPU, memory, storage and process I/O fixture; run alone in a lane"]
    fn attribution_full_retention_budget() {
        let dir = Temp::new();
        let mut store = Store::open(&dir.0).unwrap();
        let mut native = crate::performance::Collector::new();
        let machine = native.sample();
        let minute = crate::performance::Minute {
            first_wall_ms: machine.wall_ms,
            first_monotonic_ms: machine.monotonic_ms,
            last: machine.clone(),
            samples: 12,
            min_available_bytes: machine.metrics.physical_available_bytes,
            peaks: machine.metrics.clone(),
        };
        let machine_json = serde_json::to_string(&machine).unwrap();
        let minute_json = serde_json::to_string(&minute).unwrap();
        // Payloads at the hard wire limit, including the large dictionary. SQLite pages/indexes,
        // not a JSON estimate, decide whether full configured retention fits the combined cap.
        let compact = Sampler::default().attribute(&machine, snapshot(vec![]), &[]);
        let mut json = serde_json::to_string(&Compact::from(&compact)).unwrap();
        json.push_str(&" ".repeat(1792 - json.len()));
        let mut metadata = root(1, 10, "build", "APP");
        metadata.end_ms = Some(machine.wall_ms - 86400000);
        let mut root_json = serde_json::to_string(&metadata).unwrap();
        root_json.push_str(&" ".repeat(2048 - root_json.len()));
        let mut active = metadata.clone();
        active.root = Identity { boot: machine.boot.clone().unwrap_or_default(), pid: std::process::id(), started: crate::procs::started(std::process::id()).unwrap_or_default() };
        active.id = "current-service".into();
        active.kind = "service".into();
        active.context = Context::default();
        active.end_ms = None;
        let mut active_json = serde_json::to_string(&active).unwrap();
        active_json.push_str(&" ".repeat(2048 - active_json.len()));
        let at = std::time::Instant::now();
        let tx = store.db.transaction().unwrap();
        {
            let mut a = tx.prepare("INSERT INTO samples(wall_ms,json) VALUES(?1,?2)").unwrap();
            let mut b = tx.prepare("INSERT INTO attribution(wall_ms,session,monotonic_ms,json) VALUES(?1,'session',?1,?2)").unwrap();
            for i in 0..17280 {
                let wall = machine.wall_ms - (17279 - i) * 5000;
                a.execute(params![wall, machine_json]).unwrap();
                b.execute(params![wall, json]).unwrap();
            }
        }
        {
            let mut a = tx.prepare("INSERT INTO minutes(wall_ms,json) VALUES(?1,?2)").unwrap();
            let mut b = tx.prepare("INSERT INTO attribution_minutes(seq,wall_ms,json) VALUES(?1,?1,?2)").unwrap();
            for i in 0..43200 {
                let wall = machine.wall_ms - (43199 - i) * 60000;
                a.execute(params![wall, minute_json]).unwrap();
                b.execute(params![wall, json]).unwrap();
            }
        }
        {
            let mut a = tx.prepare("INSERT INTO workloads(id,json,ended) VALUES(?1,?2,?3)").unwrap();
            a.execute(params![active.id, active_json, Option::<i64>::None]).unwrap();
            for i in 1..8192 {
                a.execute(params![format!("workload-{i:08}"), root_json, Some(1i64)]).unwrap();
            }
        }
        tx.commit().unwrap();
        let bytes = std::fs::metadata(dir.0.join("history.sqlite")).unwrap().len();
        eprintln!("full machine+attribution retention: {bytes} bytes in {:?}", at.elapsed());
        assert!(bytes < crate::performance::MAX_DATABASE_BYTES);
        let at = std::time::Instant::now();
        #[cfg(windows)]
        let before = crate::procs::recorder_cost().unwrap();
        #[cfg(windows)]
        let mut peak_rss = before.1;
        #[cfg(windows)]
        let mut phases = [0u64; 4];
        let mut sampler = Sampler::default();
        let mut minute_id = None;
        let mut minute = None;
        let settings = store.settings().unwrap();
        assert_eq!(store.db.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0)).unwrap(), 2, "measure FULL-synchronous durability");
        // Exclude fixture creation, logging and cleanup; include both durable transactions,
        // native sampling and retention pruning. The ABI matches the existing Windows collector.
        #[cfg(windows)]
        let io_before = crate::procs::recorder_io().unwrap();
        for _ in 0..12 {
            #[cfg(windows)]
            let t0 = crate::procs::recorder_cost().unwrap().0;
            let sample = native.sample();
            #[cfg(windows)]
            let t1 = crate::procs::recorder_cost().unwrap().0;
            let mut attributed = sampler.collect(&store, &sample).unwrap();
            #[cfg(windows)]
            let t2 = crate::procs::recorder_cost().unwrap().0;
            let new_minute = minute_id.is_none();
            if let Some(minute) = minute.as_mut() {
                crate::performance::Minute::add(minute, &sample);
            } else {
                minute = Some(crate::performance::Minute::new(&sample));
            }
            minute_id = Some(store.append(&sample, minute.as_ref().unwrap(), minute_id, &settings).unwrap());
            #[cfg(windows)]
            let t3 = crate::procs::recorder_cost().unwrap().0;
            store.append_attribution(&mut attributed, new_minute).unwrap();
            #[cfg(windows)]
            {
                let t4 = crate::procs::recorder_cost().unwrap().0;
                for (sum, diff) in phases.iter_mut().zip([t1 - t0, t2 - t1, t3 - t2, t4 - t3]) {
                    *sum += diff;
                }
            }
            #[cfg(windows)]
            {
                peak_rss = peak_rss.max(crate::procs::recorder_cost().unwrap().1);
            }
        }
        #[cfg(windows)]
        let io_after = crate::procs::recorder_io().unwrap();
        eprintln!("12 full-retention combined native collections and durable writes: {:?}", at.elapsed());
        #[cfg(windows)]
        {
            let after = crate::procs::recorder_cost().unwrap();
            let cpu_ms = (after.0 - before.0) as f64 / 10000.;
            let rss = peak_rss.saturating_sub(before.1);
            eprintln!("CPU phase milliseconds: machine/attribution/machine-write/attribution-write {:?}", phases.map(|p| p as f64 / 10000.));
            eprintln!("Combined recorder: {cpu_ms:.1} ms process CPU per 12 five-second intervals; {rss} additional sampled RSS bytes");
            let io: [u64; 6] = std::array::from_fn(|i| io_after[i].checked_sub(io_before[i]).unwrap());
            eprintln!("Combined process I/O over12cycles: read {} bytes/{} operations; written {} bytes/{} operations; other {} bytes/{} operations", io[3], io[0], io[4], io[1], io[5], io[2]);
            // At the normal five-second cadence these are at most 205KiB/s reads and 51KiB/s
            // writes, including rollback-journal amplification. They are process transfers,
            // not physical disk traffic, flush counts or storage latency.
            assert!(io[4] > 0 && io[1] > 0, "measure actual durable writes");
            assert!(io[3] <= 12 * 1024 * 1024, "combined recorder process-read budget: {} bytes per12cycles", io[3]);
            assert!(io[4] <= 3 * 1024 * 1024, "combined recorder process-write budget: {} bytes per12cycles", io[4]);
            assert!(cpu_ms < 600., "combined collection should consume under 1% of one core over twelve five-second intervals");
            assert!(rss < 32 * 1024 * 1024, "combined recorder should add under32MiB resident memory");
        }
    }
}
