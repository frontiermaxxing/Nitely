//! Bounded cached machine-history queries shared by the app, CLI and MCP.
use crate::performance::{self, Metrics, Minute, Sample, Session, Settings, Store};
use crate::performance_attribution::{AttributionSample, Workload};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_limit() -> usize { 1000 }
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View { #[default] Now, History, Episodes }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Query {
    pub view: View,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub before_last_restart: bool,
    pub limit: usize,
    pub project: Option<String>,
}
impl Default for Query {
    fn default() -> Self { Self { view: View::Now, from_ms: None, to_ms: None, before_last_restart: false, limit: default_limit(), project: None } }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Point {
    pub from_ms: i64,
    pub to_ms: i64,
    pub values: Metrics,
    pub peaks: Metrics,
    pub min_available_bytes: Option<u64>,
    pub observations: u32,
    /// `sample`, `minute`, or `bucket`; peaks are never represented as sustained values.
    pub resolution: String,
    pub contains_gap: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gap { pub from_ms: i64, pub to_ms: i64, pub kind: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub from_ms: i64, pub to_ms: i64, pub observations: usize,
    pub observed: Vec<String>,
    /// Always separate from observations. Temporal overlap alone establishes no cause.
    pub inferred_causes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub gaps: Vec<Gap>,
    pub bounded: bool,
    pub gaps_truncated: bool,
    pub sessions_truncated: bool,
    pub events_truncated: bool,
    pub attribution_truncated: bool,
    pub workloads_truncated: bool,
    pub attribution_retained_bounds_ms: Option<(i64, i64)>,
    pub attribution_first_returned_ms: Option<i64>,
    pub source_points: usize,
    pub returned_points: usize,
    pub resolution: String,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub as_of_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    pub range_anchor: String,
    pub project: Option<String>,
    pub status: String,
    pub age_ms: Option<i64>,
    pub latest: Option<Sample>,
    pub settings: Settings,
    /// Current configuration using the cached machine total, not historical policy or permission to start work.
    pub admission_limits: Option<crate::machine::Limits>,
    pub points: Vec<Point>,
    pub episodes: Vec<Episode>,
    pub events: Vec<crate::performance_events::Event>,
    pub attribution_latest: Option<AttributionSample>,
    pub attribution_age_ms: Option<i64>,
    pub attribution: Vec<AttributionSample>,
    pub workloads: Vec<Workload>,
    pub related_issues: std::collections::BTreeMap<String, String>,
    pub sessions: Vec<Session>,
    pub coverage: Coverage,
    pub units: serde_json::Value,
}

pub fn query(dir: &Path, request: &Query) -> Result<Report> { query_at(dir, request, performance::wall_ms()) }
pub fn query_at(dir: &Path, request: &Query, now: i64) -> Result<Report> {
    if request.limit == 0 || request.limit > 2000 { bail!("performance limit must be 1–2000"); }
    if request.project.as_ref().is_some_and(|s| s.is_empty() || s.len() > 64 || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) { bail!("invalid project key"); }
    if request.before_last_restart && (request.from_ms.is_some() || request.to_ms.is_some()) { bail!("before_last_restart cannot be combined with an explicit range"); }
    let mut to = request.to_ms.unwrap_or(now);
    let mut from = request.from_ms.unwrap_or_else(|| to.saturating_sub(if request.view == View::Now { 300_000 } else { 3_600_000 }));
    let mut anchor = "requested_wall_time".to_string();
    let store = Store::read(dir)?;
    let settings = store.as_ref().map(Store::settings).transpose()?.unwrap_or_default();
    let sessions = store.as_ref().map(Store::sessions).transpose()?.unwrap_or_default();
    if request.before_last_restart {
        let boot = previous_boot(&sessions).ok_or_else(|| anyhow::anyhow!("no recorded boot transition; a collection gap alone does not establish a restart"))?;
        let previous = latest_for_boot(store.as_ref().unwrap(), &boot)?.ok_or_else(|| anyhow::anyhow!("the previous boot's samples have expired"))?;
        to = previous.wall_ms;
        from = to.saturating_sub(600_000);
        anchor = "last_recorded_sample_of_previous_boot".into();
    }
    if from < 0 || to < from || to.saturating_sub(from) > 31 * 86_400_000 { bail!("performance range must be ordered UTC milliseconds spanning at most 31 days"); }
    let latest = store.as_ref().map(|s| s.samples(i64::MIN, i64::MAX, 1)).transpose()?.unwrap_or_default().into_iter().next();
    let age = latest.as_ref().map(|s| now.saturating_sub(s.wall_ms));
    let status = if !settings.enabled { "paused" } else { match age { None => "unavailable", Some(a) if a < -2000 => "clock_changed", Some(a) if a <= 15_000 => "fresh", _ => "stale" } };
    let use_minutes = to - from > 7_200_000 || latest.as_ref().is_some_and(|s| from < s.wall_ms.saturating_sub(i64::from(settings.detailed_hours) * 3_600_000));
    let mut raw_samples = Vec::new();
    let mut points = Vec::new();
    let mut gaps = Vec::new();
    if let Some(store) = &store {
        if use_minutes {
            let mut minutes = store.minutes(from, to, 43200)?;
            minutes.reverse();
            for pair in minutes.windows(2) { if let Some(gap) = sample_gap(&pair[0].last, &pair[1].last, 90_000) { gaps.push(gap); } }
            for minute in &minutes {
                if minute.last.monotonic_ms.saturating_sub(minute.first_monotonic_ms) > u64::from(minute.samples.saturating_sub(1)) * performance::CADENCE_MS + 10_000 {
                    gaps.push(Gap { from_ms: minute.first_wall_ms, to_ms: minute.last.wall_ms, kind: "incomplete_minute".into() });
                }
            }
            points = minutes.into_iter().map(point_of_minute).collect();
            raw_samples = store.samples(from, to, 17280)?;
            raw_samples.reverse();
        } else {
            raw_samples = store.samples(from, to, 17280)?;
            raw_samples.reverse();
            for pair in raw_samples.windows(2) { if let Some(gap) = sample_gap(&pair[0], &pair[1], 15_000) { gaps.push(gap); } }
            points = raw_samples.iter().map(|s| Point { from_ms: s.wall_ms, to_ms: s.wall_ms, values: s.metrics.clone(), peaks: s.metrics.clone(), min_available_bytes: s.metrics.physical_available_bytes, observations: 1, resolution: "sample".into(), contains_gap: false }).collect();
        }
    }
    if points.first().is_none_or(|p| p.from_ms > from.saturating_add(if use_minutes { 90_000 } else { 15_000 })) {
        gaps.push(Gap { from_ms: from, to_ms: points.first().map_or(to, |p| p.from_ms), kind: "no_retained_samples".into() });
    }
    if let Some(last) = points.last().filter(|p| to - p.to_ms > if use_minutes { 90_000 } else { 15_000 }) { gaps.push(Gap { from_ms: last.to_ms, to_ms: to, kind: "no_retained_samples".into() }); }
    for point in &mut points { point.contains_gap = gaps.iter().any(|g| g.from_ms < point.to_ms && g.to_ms > point.from_ms); }
    let source_points = points.len();
    let points = if request.view == View::History { compact(points, request.limit, &gaps) } else { vec![] };
    let episodes = episodes(&raw_samples);
    let mut notes = vec!["Machine totals always cover the whole PC; project filters apply only to workload attribution and events.".into(), "Null counters mean unavailable, not zero; overlapping work is not proof of causation.".into()];
    if use_minutes { notes.push("Minute extrema preserve observed peaks; they do not establish sustained pressure within a minute.".into()); }
    notes.push("At most 100 recent sustained episodes are returned, covering retained detailed samples only; absence outside detailed retention is not evidence of no pressure.".into());
    if anchor != "requested_wall_time" { notes.push("This window ends at the last retained sample of the previous boot, not a known shutdown instant.".into()); }
    let returned_points = points.len();
    let selected_sessions: Vec<Session> = sessions.into_iter().filter(|s| s.started_ms <= to && s.stopped_ms.is_none_or(|end| end >= from)).collect();
    let sessions_truncated = selected_sessions.len() > 200;
    let gaps_truncated = gaps.len() > 200;
    let admission_limits = latest.as_ref().and_then(|s| s.metrics.physical_total_bytes).map(crate::machine::limits_for_recorded_total);
    let (events, events_truncated) = store.as_ref().map(|s| crate::performance_events::read(s, from, to, request.project.as_deref(), request.limit)).transpose()?.unwrap_or_default();
    let attribution_latest = store.as_ref().map(|s| s.attribution_latest(request.project.as_deref())).transpose()?.flatten();
    let attribution_age_ms = attribution_latest.as_ref().map(|s| now.saturating_sub(s.wall_ms));
    let attribution_limit = request.limit.min(200);
    let mut attribution = if request.view == View::Now { vec![] } else { store.as_ref().map(|s| s.attribution(from, to, attribution_limit+1, request.project.as_deref())).transpose()?.unwrap_or_default() };
    let attribution_truncated = attribution.len() > attribution_limit;
    attribution.truncate(attribution_limit);
    attribution.reverse();
    let attribution_first_returned_ms = attribution.first().map(|s| s.wall_ms);
    let attribution_retained_bounds_ms = store.as_ref().map(Store::attribution_bounds).transpose()?.flatten();
    let mut workloads = store.as_ref().map(|s| s.recent_workloads(from, to, 201, request.project.as_deref())).transpose()?.unwrap_or_default();
    let workloads_truncated = workloads.len() > 200;
    workloads.truncate(200);
    let related_issues = events.iter().filter_map(|e| e.issue.as_ref())
        .chain(workloads.iter().filter_map(|w| w.context.issue.as_ref()))
        .chain(attribution.iter().chain(attribution_latest.iter()).flat_map(|s| s.workloads.iter().filter_map(|w| w.context.issue.as_ref())))
        .map(|key| (key.clone(), crate::links::issue_url(key))).collect();
    if attribution_truncated { notes.push(format!("Attribution is limited to the latest {attribution_limit} observations in the selected range; earlier machine-chart peaks may have contributors outside this response. Narrow the range to inspect those intervals.")); }
    notes.push("Process working-set sums can overlap and are not physical RAM. Process I/O includes nondisk activity; minute attribution values are per-metric peaks with interval I/O totals, not simultaneous readings.".into());
    Ok(Report { as_of_ms: now, from_ms: from, to_ms: to, range_anchor: anchor, project: request.project.clone(), status: status.into(), age_ms: age, latest, settings, admission_limits,
        points, episodes, events, attribution_latest, attribution_age_ms, attribution, workloads, related_issues, sessions: selected_sessions.into_iter().take(200).collect(),
        coverage: Coverage { gaps: gaps.into_iter().take(200).collect(), bounded: source_points > request.limit, gaps_truncated, sessions_truncated, events_truncated, attribution_truncated, workloads_truncated, attribution_retained_bounds_ms, attribution_first_returned_ms, source_points, returned_points, resolution: if use_minutes { "minute" } else { "sample" }.into(), notes },
        units: serde_json::json!({ "wall_time": "UTC milliseconds since Unix epoch", "memory": "bytes", "cpu": "percent of machine capacity", "paging": "pages/second", "disk_throughput": "bytes/second", "disk_latency": "seconds/transfer", "disk_queue": "requests", "monotonic": "milliseconds; boot-relative on Windows, collector-relative elsewhere" }) })
}

fn previous_boot(sessions: &[Session]) -> Option<String> {
    let mut known = sessions.iter().filter_map(|s| s.boot.as_ref());
    let latest = known.next()?;
    known.find(|boot| *boot != latest).cloned()
}
fn latest_for_boot(store: &Store, boot: &str) -> Result<Option<Sample>> {
    // Bounded retained tables; CASE shields malformed JSON. Read at most one complete record
    // from each resolution, without assuming chronological wall time across clock changes.
    let mut candidates = Vec::new();
    for (table, path, minute) in [("samples", "$.boot", false), ("minutes", "$.last.boot", true)] {
        let sql = format!("SELECT json FROM {table} WHERE CASE WHEN json_valid(json) THEN json_extract(json,?1) END=?2 ORDER BY id DESC LIMIT 8");
        let mut stmt = store.db.prepare(&sql)?;
        for row in stmt.query_map(rusqlite::params![path, boot], |r| r.get::<_, String>(0))? {
            let json = row?;
            let sample = if minute { serde_json::from_str::<Minute>(&json).ok().map(|m| m.last) } else { serde_json::from_str::<Sample>(&json).ok() };
            if let Some(sample) = sample { candidates.push(sample); break; }
        }
    }
    Ok(candidates.into_iter().max_by_key(|s| s.monotonic_ms))
}
fn sample_gap(a: &Sample, b: &Sample, cadence: u64) -> Option<Gap> {
    let delta = b.monotonic_ms.saturating_sub(a.monotonic_ms);
    let known_restart = a.boot.is_some() && b.boot.is_some() && a.boot != b.boot;
    let same_clock = a.boot.is_some() && a.boot == b.boot || a.session == b.session;
    let kind = if known_restart { "restart" }
        else if !same_clock { "collector_changed" }
        else if (b.wall_ms - a.wall_ms - delta as i64).unsigned_abs() > 2000 { "clock_change" }
        else if delta > cadence { "collection_gap" } else { return None };
    Some(Gap { from_ms: a.wall_ms.min(b.wall_ms), to_ms: a.wall_ms.max(b.wall_ms), kind: kind.into() })
}
fn point_of_minute(m: Minute) -> Point { Point { from_ms: m.first_wall_ms, to_ms: m.last.wall_ms, values: m.last.metrics, peaks: m.peaks, min_available_bytes: m.min_available_bytes, observations: m.samples, resolution: "minute".into(), contains_gap: false } }
fn max_metrics(into: &mut Metrics, from: &Metrics) {
    macro_rules! peak { ($($field:ident),*) => { $(if let Some(v) = from.$field { into.$field = Some(into.$field.map_or(v, |old| old.max(v))); })* }; }
    peak!(cpu_percent, physical_total_bytes, physical_available_bytes, commit_bytes, commit_limit_bytes, kernel_paged_bytes, kernel_nonpaged_bytes,
        pages_input_per_sec, pages_output_per_sec, disk_read_bytes_per_sec, disk_write_bytes_per_sec, disk_latency_seconds, disk_queue);
}
fn compact(points: Vec<Point>, limit: usize, gaps: &[Gap]) -> Vec<Point> {
    if points.len() <= limit { return points; }
    points.chunks(points.len().div_ceil(limit)).map(|chunk| {
        let mut point = chunk.last().unwrap().clone();
        point.from_ms = chunk.iter().map(|p| p.from_ms).min().unwrap();
        point.to_ms = chunk.iter().map(|p| p.to_ms).max().unwrap();
        point.observations = 0;
        point.resolution = "bucket".into();
        for item in chunk {
            max_metrics(&mut point.peaks, &item.peaks);
            if let Some(v) = item.min_available_bytes { point.min_available_bytes = Some(point.min_available_bytes.map_or(v, |old| old.min(v))); }
            point.observations += item.observations;
            point.contains_gap |= item.contains_gap;
        }
        point.contains_gap |= gaps.iter().any(|g| g.from_ms <= point.to_ms && g.to_ms >= point.from_ms);
        point
    }).collect()
}
fn pressure(metrics: &Metrics) -> Vec<String> {
    let mut out = Vec::new();
    if metrics.cpu_percent.is_some_and(|v| v >= 90.0) { out.push("CPU at least 90%".into()); }
    if matches!((metrics.physical_available_bytes, metrics.physical_total_bytes), (Some(a),Some(t)) if t>0 && a as f64/t as f64<=0.1) { out.push("available physical memory at most 10%".into()); }
    if matches!((metrics.commit_bytes, metrics.commit_limit_bytes), (Some(a),Some(t)) if t>0 && a as f64/t as f64>=0.9) { out.push("system commit at least 90% of limit".into()); }
    out
}
fn episodes(samples: &[Sample]) -> Vec<Episode> {
    let mut out = Vec::new();
    let mut active = std::collections::BTreeMap::<String, usize>::new();
    let flush = |out: &mut Vec<Episode>, start: usize, end: usize, reason: String| {
        if end > start && samples[end].monotonic_ms.saturating_sub(samples[start].monotonic_ms) >= 15_000 {
            out.push(Episode { from_ms: samples[start].wall_ms, to_ms: samples[end].wall_ms, observations: end-start+1, observed: vec![reason], inferred_causes: vec![] });
        }
    };
    for (index, sample) in samples.iter().enumerate() {
        let observed = pressure(&sample.metrics);
        let gap = index > 0 && sample_gap(&samples[index-1], sample, 15_000).is_some();
        let ended: Vec<String> = active.keys().filter(|reason| gap || !observed.contains(reason)).cloned().collect();
        for reason in ended {
            let start = active.remove(&reason).unwrap();
            if index > 0 { flush(&mut out, start, index-1, reason); }
        }
        for reason in observed { active.entry(reason).or_insert(index); }
    }
    if !samples.is_empty() { for (reason, start) in active { flush(&mut out, start, samples.len()-1, reason); } }
    out.sort_by_key(|e| e.to_ms);
    out.into_iter().rev().take(100).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: i64 = 1_800_000_000_000;
    struct Fixture { dir: std::path::PathBuf, store: Store }
    impl Fixture {
        fn new() -> Self { let dir = crate::paths::unique_temp("performance-query", ""); let store = Store::open(&dir).unwrap(); Self { dir, store } }
        fn sample(&self, at: i64, boot: Option<&str>, session: &str, cpu: Option<f64>) -> Sample {
            let sample = Sample { wall_ms: at, monotonic_ms: at.max(0) as u64, boot: boot.map(str::to_string), session: session.into(),
                metrics: Metrics { cpu_percent: cpu, physical_total_bytes: Some(100), physical_available_bytes: Some(50), ..Metrics::default() }, collection_ms: 1 };
            self.store.db.execute("INSERT INTO samples(wall_ms,json) VALUES(?1,?2)", rusqlite::params![at, serde_json::to_string(&sample).unwrap()]).unwrap();
            sample
        }
        fn query(&self, query: Query) -> Report { query_at(&self.dir, &query, NOW).unwrap() }
    }
    impl Drop for Fixture { fn drop(&mut self) { let db = std::mem::replace(&mut self.store.db, rusqlite::Connection::open_in_memory().unwrap()); drop(db); let _ = std::fs::remove_dir_all(&self.dir); } }
    #[test]
    fn reads_do_not_create_a_recorder_or_store_and_validate_bounds() {
        let dir = crate::paths::unique_temp("performance-read-only", "");
        let report = query_at(&dir, &Query::default(), NOW).unwrap();
        assert_eq!(report.status, "unavailable");
        assert!(!dir.exists());
        assert!(!report.coverage.gaps.is_empty());
        for query in [Query { limit: 0, ..Query::default() }, Query { limit: 2001, ..Query::default() }, Query { from_ms: Some(NOW), to_ms: Some(NOW-1), ..Query::default() }, Query { from_ms: Some(0), to_ms: Some(NOW), ..Query::default() }] {
            assert!(query_at(&dir, &query, NOW).is_err());
        }
    }
    #[test]
    fn before_restart_finds_previous_boot_after_workers_are_gone() {
        let fixture = Fixture::new();
        fixture.store.start_session("old", Some("boot-a"), NOW-1_000_000).unwrap();
        fixture.sample(NOW-700_000, Some("boot-a"), "old", Some(95.0));
        fixture.sample(NOW-100_000, Some("boot-a"), "old", Some(96.0));
        fixture.store.start_session("unknown", None, NOW-50_000).unwrap();
        fixture.store.start_session("new", Some("boot-b"), NOW-20_000).unwrap();
        fixture.sample(NOW-10_000, Some("boot-b"), "new", Some(10.0));
        let report = fixture.query(Query { view: View::History, before_last_restart: true, ..Query::default() });
        assert_eq!(report.to_ms, NOW-100_000);
        assert_eq!(report.from_ms, NOW-700_000);
        assert_eq!(report.points.len(), 2);
        assert_eq!(report.range_anchor, "last_recorded_sample_of_previous_boot");
        assert_eq!(report.latest.unwrap().boot.as_deref(), Some("boot-b"));
    }
    #[test]
    fn an_unknown_boot_or_gap_is_not_reported_as_restart() {
        let fixture = Fixture::new();
        fixture.store.start_session("old", None, NOW-100_000).unwrap();
        fixture.sample(NOW-100_000, None, "old", None);
        fixture.store.start_session("new", None, NOW-10_000).unwrap();
        fixture.sample(NOW-10_000, None, "new", None);
        assert!(query_at(&fixture.dir, &Query { before_last_restart: true, ..Query::default() }, NOW).is_err());
        let report = fixture.query(Query { view: View::History, ..Query::default() });
        assert!(report.coverage.gaps.iter().any(|g| g.kind == "collector_changed"));
        assert!(report.points.iter().all(|p| p.values.cpu_percent.is_none()));
    }
    #[test]
    fn pressure_needs_sustained_samples_and_does_not_bridge_a_gap() {
        let fixture = Fixture::new();
        for seconds in [90,85,80,75,30,25] { fixture.sample(NOW-seconds*1000, Some("boot"), "s", Some(95.0)); }
        let report = fixture.query(Query { view: View::Episodes, ..Query::default() });
        assert_eq!(report.episodes.len(), 1);
        assert_eq!(report.episodes[0].observations, 4);
        assert!(report.episodes[0].inferred_causes.is_empty());
        assert!(report.points.is_empty(), "episode query stays compact");
        let mut alternating: Vec<Sample> = (0..8).map(|n| Sample { wall_ms: NOW+n*5000, monotonic_ms: (n*5000) as u64, boot: Some("boot".into()), session: "s".into(), metrics: Metrics { cpu_percent: Some(if n%2 == 0 {95.0} else {10.0}), physical_total_bytes: Some(100), physical_available_bytes: Some(if n%2 == 0 {50} else {5}), ..Default::default() }, collection_ms: 1 }).collect();
        assert!(episodes(&alternating).is_empty(), "alternating short pressures cannot establish a sustained metric");
        for sample in &mut alternating { sample.metrics.cpu_percent = Some(95.0); }
        assert_eq!(episodes(&alternating).len(), 1);
    }
    #[test]
    fn history_compaction_preserves_spikes_and_current_reads_stay_small() {
        let fixture = Fixture::new();
        for n in 0..100 { fixture.sample(NOW-500_000+n*5000, Some("boot"), "s", Some(if n==51 {99.0} else {10.0})); }
        let report = fixture.query(Query { view: View::History, from_ms: Some(NOW-500_000), limit: 5, ..Query::default() });
        assert_eq!(report.points.len(), 5);
        assert!(report.coverage.bounded);
        assert!(report.points.iter().any(|p| p.peaks.cpu_percent == Some(99.0)));
        assert!(fixture.query(Query::default()).points.is_empty());
    }
    #[test]
    fn a_window_straddling_detail_expiry_uses_retained_minutes() {
        let fixture = Fixture::new();
        fixture.sample(NOW, Some("boot"), "s", Some(10.0));
        for at in [NOW-88_200_000, NOW-84_600_000] {
            let last = Sample { wall_ms: at, monotonic_ms: at as u64, boot: Some("boot".into()), session: "s".into(), metrics: Metrics { cpu_percent: Some(95.0), ..Default::default() }, collection_ms: 1 };
            let minute = Minute { first_wall_ms: at, first_monotonic_ms: at as u64, peaks: last.metrics.clone(), last, samples: 1, min_available_bytes: None };
            fixture.store.db.execute("INSERT INTO minutes(wall_ms,json) VALUES(?1,?2)", rusqlite::params![at,serde_json::to_string(&minute).unwrap()]).unwrap();
        }
        let report = fixture.query(Query { view: View::History, from_ms: Some(NOW-88_200_000), to_ms: Some(NOW-84_600_000), ..Default::default() });
        assert_eq!(report.coverage.resolution, "minute");
        assert_eq!(report.points.len(), 2);
        assert_eq!(report.points[0].from_ms, NOW-88_200_000);
    }
    #[test]
    fn restart_queries_keep_exited_workload_context_and_filter_projects_without_changing_totals() {
        use crate::performance_attribution::{Context, Coverage, Identity, Usage, WorkloadUsage};
        let mut fixture = Fixture::new();
        fixture.store.start_session("old", Some("boot-a"), NOW-700_000).unwrap();
        fixture.sample(NOW-60_000, Some("boot-a"), "old", Some(95.0));
        fixture.store.start_session("new", Some("boot-b"), NOW-20_000).unwrap();
        fixture.sample(NOW-10_000, Some("boot-b"), "new", Some(10.0));
        let mut usages = vec![];
        for (pid, project) in [(1,"APP"),(2,"DEMO")] {
            let context = Context { project: Some(project.into()), issue: Some(format!("{project}-1")), run_id: Some(pid.into()), ..Default::default() };
            let workload = Workload { id: format!("job-{pid}"), kind: "build".into(), context: context.clone(), root: Identity { boot: "boot-a".into(), pid, started: "1".into() }, start_ms: NOW-600_000, end_ms: Some(NOW-30_000), end_observed: false, job_accounting: None };
            fixture.store.db.execute("INSERT INTO workloads(id,json,ended) VALUES(?1,?2,?3)", rusqlite::params![workload.id,serde_json::to_string(&workload).unwrap(),workload.end_ms]).unwrap();
            usages.push(WorkloadUsage { id: workload.id, context, kind: "build".into(), usage: Usage { cpu_percent: Some(20.0), ..Default::default() } });
        }
        let mut attributed = AttributionSample { wall_ms: NOW-60_000, monotonic_ms: 600_000, session: "old".into(), boot: Some("boot-a".into()), interval_ms: Some(5000), collection_ms: 1, coverage: Coverage { supported: true, ..Default::default() }, resolution: "sample".into(), observations: 1, nitely: Usage { cpu_percent: Some(40.0), ..Default::default() }, related: Usage::default(), other: Usage::default(), unattributed_cpu_percent: Some(55.0), workloads: usages, top_other: vec![] };
        fixture.store.append_attribution(&mut attributed, true).unwrap();
        let query = |project: &str| Query { view: View::History, before_last_restart: true, project: Some(project.into()), ..Default::default() };
        let app = fixture.query(query("APP")); let demo = fixture.query(query("DEMO"));
        assert_eq!(app.workloads.len(), 1);
        assert_eq!(app.workloads[0].end_ms, Some(NOW-30_000));
        assert_eq!(app.attribution[0].workloads.len(), 1);
        assert_eq!(app.attribution[0].workloads[0].context.project.as_deref(), Some("APP"));
        assert_eq!(app.attribution[0].nitely.cpu_percent, demo.attribution[0].nitely.cpu_percent);
        assert_eq!(app.latest.unwrap().metrics, demo.latest.unwrap().metrics);
        assert_eq!(app.related_issues["APP-1"], crate::links::issue_url("APP-1"));
        assert!(!app.related_issues.contains_key("DEMO-1"));
        assert_eq!(app.attribution_age_ms, Some(60_000));
    }
    #[test]
    fn freshness_and_clock_changes_are_explicit_and_project_does_not_change_totals() {
        let fixture = Fixture::new();
        fixture.sample(NOW-30_000, Some("boot"), "s", Some(20.0));
        assert_eq!(fixture.query(Query::default()).status, "stale");
        let a = fixture.query(Query { project: Some("APP".into()), ..Query::default() });
        let b = fixture.query(Query { project: Some("DEMO".into()), ..Query::default() });
        assert_eq!(a.latest.unwrap().metrics, b.latest.unwrap().metrics);
        fixture.sample(NOW+60_000, Some("boot"), "s", Some(10.0));
        assert_eq!(fixture.query(Query::default()).status, "clock_changed");
        fixture.store.configure(&Settings { enabled: false, ..Settings::default() }).unwrap();
        assert_eq!(fixture.query(Query::default()).status, "paused");
    }
}
