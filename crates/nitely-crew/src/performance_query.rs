//! Bounded cached machine-history queries shared by the app, CLI and MCP.
use nitely_core::performance::{self, Metrics, Minute, Sample, Session, Settings, Store};
use nitely_core::performance_attribution::{AttributionSample, Workload};
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
    /// Whole summary interval extends outside the request; extrema/counts cannot be clipped.
    #[serde(default)]
    pub overlaps_range_boundary: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gap { pub from_ms: i64, pub to_ms: i64, pub kind: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub from_ms: i64, pub to_ms: i64, pub observations: usize,
    pub observed: Vec<String>,
    /// Independent per-metric maxima across this episode's detailed observations, not a
    /// simultaneous reading. Missing throughout remains null.
    #[serde(default)]
    pub peaks: Metrics,
    pub min_available_bytes: Option<u64>,
    pub min_available_percent: Option<f64>,
    pub max_commit_percent: Option<f64>,
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
    /// Number of source minute summaries extending beyond the requested range, before compaction.
    #[serde(default)]
    pub boundary_points: usize,
    pub resolution: String,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub as_of_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    pub range_anchor: String,
    pub range_boot: Option<String>,
    pub project: Option<String>,
    pub status: String,
    pub age_ms: Option<i64>,
    pub latest: Option<Sample>,
    pub settings: Settings,
    /// Current configuration using the cached machine total, not historical policy or permission to start work.
    pub admission_limits: Option<nitely_core::machine::Limits>,
    pub points: Vec<Point>,
    pub episodes: Vec<Episode>,
    pub events: Vec<nitely_core::performance_events::Event>,
    /// How hard Nitely lets builds and tests push now, and why, as last measured (NIT-519); None
    /// before the first measurement. Reading it measures nothing.
    pub pace: Option<nitely_core::pace::Pace>,
    pub pace_measured_ms: Option<i64>,
    /// Each change of pace in the range, newest first, with what was said about it.
    pub adjustments: Vec<nitely_core::pace::Adjustment>,
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
    let mut selected_boot = None;
    let store = Store::read(dir)?;
    let settings = store.as_ref().map(Store::settings).transpose()?.unwrap_or_default();
    let sessions = store.as_ref().map(Store::sessions).transpose()?.unwrap_or_default();
    if request.before_last_restart {
        let boot = previous_boot(&sessions).ok_or_else(|| anyhow::anyhow!("no recorded boot transition; a collection gap alone does not establish a restart"))?;
        let previous = latest_for_boot(store.as_ref().unwrap(), &boot)?.ok_or_else(|| anyhow::anyhow!("the previous boot's samples have expired"))?;
        to = previous.wall_ms;
        from = to.saturating_sub(600_000);
        anchor = "last_recorded_sample_of_previous_boot".into();
        selected_boot = Some(boot);
    }
    if from < 0 || to < from || to.saturating_sub(from) > 31 * 86_400_000 { bail!("performance range must be ordered UTC milliseconds spanning at most 31 days"); }
    let latest = store.as_ref().map(|s| s.samples(i64::MIN, i64::MAX, 1)).transpose()?.unwrap_or_default().into_iter().next();
    let age = latest.as_ref().map(|s| now.saturating_sub(s.wall_ms));
    let status = if !settings.enabled { "paused" } else { match age { None => "unavailable", Some(a) if a < -2000 => "clock_changed", Some(a) if a <= 15_000 => "fresh", _ => "stale" } };
    let mut use_minutes = to - from > 7_200_000 || latest.as_ref().is_some_and(|s| from < s.wall_ms.saturating_sub(i64::from(settings.detailed_hours) * 3_600_000));
    let same_boot = |boot: Option<&str>| selected_boot.as_deref().is_none_or(|selected| boot == Some(selected));
    let mut raw_samples = Vec::new();
    let mut points = Vec::new();
    let mut gaps = Vec::new();
    if let Some(store) = &store {
        raw_samples = store.samples(from, to, 17280)?;
        raw_samples.retain(|sample| same_boot(sample.boot.as_deref()));
        raw_samples.reverse();
        // Row-cap retention can expire a prior boot even when a backward clock makes its
        // wall time appear recent. Retained minute history must remain available then.
        if selected_boot.is_some() && raw_samples.is_empty() { use_minutes = true; }
        if use_minutes {
            let mut minutes = store.minutes(from, to, 43200)?;
            minutes.retain(|minute| same_boot(minute.last.boot.as_deref()));
            minutes.reverse();
            for pair in minutes.windows(2) { if let Some(gap) = sample_gap(&pair[0].last, &pair[1].last, 90_000) { gaps.push(gap); } }
            for minute in &minutes {
                if minute.last.monotonic_ms.saturating_sub(minute.first_monotonic_ms) > u64::from(minute.samples.saturating_sub(1)) * performance::CADENCE_MS + 10_000 {
                    gaps.push(Gap { from_ms: minute.first_wall_ms, to_ms: minute.last.wall_ms, kind: "incomplete_minute".into() });
                }
            }
            points = minutes.into_iter().map(point_of_minute).collect();
        } else {
            for pair in raw_samples.windows(2) { if let Some(gap) = sample_gap(&pair[0], &pair[1], 15_000) { gaps.push(gap); } }
            points = raw_samples.iter().map(|s| Point { from_ms: s.wall_ms, to_ms: s.wall_ms, values: s.metrics.clone(), peaks: s.metrics.clone(), min_available_bytes: s.metrics.physical_available_bytes, observations: 1, resolution: "sample".into(), contains_gap: false, overlaps_range_boundary: false }).collect();
        }
    }
    if points.first().is_none_or(|p| p.from_ms > from.saturating_add(if use_minutes { 90_000 } else { 15_000 })) {
        gaps.push(Gap { from_ms: from, to_ms: points.first().map_or(to, |p| p.from_ms), kind: "no_retained_samples".into() });
    }
    if let Some(last) = points.last().filter(|p| to - p.to_ms > if use_minutes { 90_000 } else { 15_000 }) { gaps.push(Gap { from_ms: last.to_ms, to_ms: to, kind: "no_retained_samples".into() }); }
    for point in &mut points {
        point.contains_gap = gaps.iter().any(|g| g.from_ms < point.to_ms && g.to_ms > point.from_ms);
        point.overlaps_range_boundary = point.from_ms < from || point.to_ms > to;
    }
    let boundary_points = points.iter().filter(|p| p.overlaps_range_boundary).count();
    let source_points = points.len();
    let points = if request.view == View::History { compact(points, request.limit, &gaps) } else { vec![] };
    let episodes = episodes(&raw_samples);
    let mut notes = vec!["Machine totals always cover the whole PC; project filters apply only to workload attribution and events.".into(), "Null counters mean unavailable, not zero; overlapping work is not proof of causation.".into()];
    if use_minutes { notes.push("Minute extrema preserve observed peaks; they do not establish sustained pressure within a minute.".into()); }
    if boundary_points > 0 { notes.push(format!("{boundary_points} minute summaries overlap a range boundary. Their actual full intervals, last values, extrema and observation counts are returned; peaks may lie outside the requested range and exact subrange values cannot be recovered from these summaries.")); }
    notes.push("At most 100 recent sustained episodes are returned, covering retained detailed samples only; absence outside detailed retention is not evidence of no pressure.".into());
    if selected_boot.is_some() { notes.push("This window contains only the selected previous boot and ends at its last retained sample, not a known shutdown instant. Events without a recorded boot identity are omitted because wall time alone cannot assign them across a clock change. Latest/current fields still describe current recording.".into()); }
    let returned_points = points.len();
    let selected_sessions: Vec<Session> = sessions.into_iter().filter(|s| same_boot(s.boot.as_deref()) && s.started_ms <= to && s.stopped_ms.is_none_or(|end| end >= from)).collect();
    let sessions_truncated = selected_sessions.len() > 200;
    let gaps_truncated = gaps.len() > 200;
    let admission_limits = latest.as_ref().and_then(|s| s.metrics.physical_total_bytes).map(nitely_core::machine::limits_for_recorded_total);
    let (events, events_truncated) = store.as_ref().map(|s| nitely_core::performance_events::read_for_boot(s, from, to, request.project.as_deref(), request.limit, selected_boot.as_deref())).transpose()?.unwrap_or_default();
    let adjustments = store.as_ref().map(|s| nitely_core::pace::adjustments(s, from, to, request.limit.min(200))).transpose()?.unwrap_or_default();
    let (pace_measured_ms, pace) = nitely_core::pace::cached_beside(dir).map_or((None, None), |(at, p)| (Some(at), Some(p)));
    let attribution_latest = store.as_ref().map(|s| s.attribution_latest(request.project.as_deref())).transpose()?.flatten();
    let attribution_age_ms = attribution_latest.as_ref().map(|s| now.saturating_sub(s.wall_ms));
    let attribution_limit = request.limit.min(200);
    let mut attribution = if request.view == View::Now { vec![] } else { store.as_ref().map(|s| s.attribution_for_boot(from, to, attribution_limit+1, request.project.as_deref(), selected_boot.as_deref())).transpose()?.unwrap_or_default() };
    let attribution_truncated = attribution.len() > attribution_limit;
    attribution.truncate(attribution_limit);
    attribution.reverse();
    let attribution_first_returned_ms = attribution.first().map(|s| s.wall_ms);
    let attribution_retained_bounds_ms = store.as_ref().map(|s| s.attribution_bounds_for_boot(selected_boot.as_deref())).transpose()?.flatten();
    let mut workloads = store.as_ref().map(|s| s.recent_workloads_for_boot(from, to, 201, request.project.as_deref(), selected_boot.as_deref())).transpose()?.unwrap_or_default();
    let workloads_truncated = workloads.len() > 200;
    workloads.truncate(200);
    let related_issues = events.iter().filter_map(|e| e.issue.as_ref())
        .chain(workloads.iter().filter_map(|w| w.context.issue.as_ref()))
        .chain(attribution.iter().chain(attribution_latest.iter()).flat_map(|s| s.workloads.iter().filter_map(|w| w.context.issue.as_ref())))
        .map(|key| (key.clone(), nitely_core::links::issue_url(key))).collect();
    if attribution_truncated { notes.push(format!("Attribution is limited to the latest {attribution_limit} observations in the selected range; earlier machine-chart peaks may have contributors outside this response. Narrow the range to inspect those intervals.")); }
    notes.push("Process working-set sums can overlap and are not physical RAM. Process I/O includes nondisk activity; minute attribution values are per-metric peaks with interval I/O totals, not simultaneous readings.".into());
    Ok(Report { as_of_ms: now, from_ms: from, to_ms: to, range_anchor: anchor, range_boot: selected_boot, project: request.project.clone(), status: status.into(), age_ms: age, latest, settings, admission_limits,
        points, episodes, events, pace, pace_measured_ms, adjustments, attribution_latest, attribution_age_ms, attribution, workloads, related_issues, sessions: selected_sessions.into_iter().take(200).collect(),
        coverage: Coverage { gaps: gaps.into_iter().take(200).collect(), bounded: source_points > request.limit, gaps_truncated, sessions_truncated, events_truncated, attribution_truncated, workloads_truncated, attribution_retained_bounds_ms, attribution_first_returned_ms, source_points, returned_points, boundary_points, resolution: if use_minutes { "minute" } else { "sample" }.into(), notes },
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
fn point_of_minute(m: Minute) -> Point { Point { from_ms: m.first_wall_ms, to_ms: m.last.wall_ms, values: m.last.metrics, peaks: m.peaks, min_available_bytes: m.min_available_bytes, observations: m.samples, resolution: "minute".into(), contains_gap: false, overlaps_range_boundary: false } }
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
            point.overlaps_range_boundary |= item.overlaps_range_boundary;
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
            let mut peaks = Metrics::default();
            let mut min_available_bytes: Option<u64> = None;
            let mut min_available_percent: Option<f64> = None;
            let mut max_commit_percent: Option<f64> = None;
            for sample in &samples[start..=end] {
                let m = &sample.metrics;
                max_metrics(&mut peaks, m);
                if let Some(v) = m.physical_available_bytes { min_available_bytes = Some(min_available_bytes.map_or(v, |old| old.min(v))); }
                if let (Some(a), Some(t)) = (m.physical_available_bytes, m.physical_total_bytes) {
                    if t > 0 { let v = 100.0*a as f64/t as f64; min_available_percent = Some(min_available_percent.map_or(v, |old| old.min(v))); }
                }
                if let (Some(a), Some(t)) = (m.commit_bytes, m.commit_limit_bytes) {
                    if t > 0 { let v = 100.0*a as f64/t as f64; max_commit_percent = Some(max_commit_percent.map_or(v, |old| old.max(v))); }
                }
            }
            out.push(Episode { from_ms: samples[start].wall_ms, to_ms: samples[end].wall_ms, observations: end-start+1, observed: vec![reason], peaks, min_available_bytes, min_available_percent, max_commit_percent, inferred_causes: vec![] });
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
        fn new() -> Self { let dir = nitely_core::paths::unique_temp("performance-query", ""); let store = Store::open(&dir).unwrap(); Self { dir, store } }
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
        let dir = nitely_core::paths::unique_temp("performance-read-only", "");
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
    fn completed_episodes_return_measured_extrema_without_chart_points() {
        let fixture = Fixture::new();
        for (n, (cpu, available, total, commit, limit)) in [
            (90.0, Some(10), 100, Some(95), 100), (100.0, Some(4), 200, Some(180), 200),
            (95.0, None, 100, Some(99), 100), (91.0, Some(8), 100, None, 0),
            (5.0, Some(100), 100, None, 0),
        ].into_iter().enumerate() {
            let mut sample = fixture.sample(NOW-25_000+n as i64*5000, Some("boot"), "s", Some(cpu));
            sample.metrics.physical_available_bytes = available;
            sample.metrics.physical_total_bytes = Some(total);
            sample.metrics.commit_bytes = commit;
            sample.metrics.commit_limit_bytes = Some(limit);
            sample.metrics.disk_queue = Some(if n == 1 { 7.0 } else if n == 4 { 99.0 } else { 1.0 });
            fixture.store.db.execute("UPDATE samples SET json=?1 WHERE wall_ms=?2", rusqlite::params![serde_json::to_string(&sample).unwrap(),sample.wall_ms]).unwrap();
        }
        let report = fixture.query(Query { view: View::Episodes, ..Default::default() });
        assert!(report.points.is_empty());
        assert_eq!(report.episodes.len(), 1);
        let episode = &report.episodes[0];
        assert_eq!((episode.from_ms, episode.to_ms, episode.observations), (NOW-25_000, NOW-10_000, 4));
        assert_eq!(episode.peaks.cpu_percent, Some(100.0));
        assert_eq!(episode.peaks.commit_bytes, Some(180));
        assert_eq!(episode.peaks.disk_queue, Some(7.0), "do not include measurements after the episode");
        assert_eq!(episode.peaks.kernel_nonpaged_bytes, None, "unavailable stays null");
        assert_eq!(episode.min_available_bytes, Some(4));
        assert_eq!(episode.min_available_percent, Some(2.0));
        assert_eq!(episode.max_commit_percent, Some(99.0), "ratios use simultaneous measurements, not independent maxima");
        assert!(episode.inferred_causes.is_empty());
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["episodes"][0]["peaks"]["cpu_percent"], 100.0);
    }

    #[test]
    fn minute_history_returns_boundary_intervals_with_explicit_uncertainty() {
        let fixture = Fixture::new();
        let from = NOW-10_800_000;
        let to = NOW;
        for (first, last, peak) in [(from-60_000, from-1, 1.0), (from-30_000, from+25_000, 95.0), (from+60_000, from+115_000, 20.0), (to-30_000, to+25_000, 100.0), (to+30_000, to+85_000, 1.0)] {
            let last = Sample { wall_ms: last, monotonic_ms: last as u64, boot: Some("boot".into()), session: "s".into(), metrics: Metrics { cpu_percent: Some(5.0), ..Default::default() }, collection_ms: 1 };
            let minute = Minute { first_wall_ms: first, first_monotonic_ms: first as u64, last, peaks: Metrics { cpu_percent: Some(peak), ..Default::default() }, min_available_bytes: Some(4), samples: 12 };
            fixture.store.db.execute("INSERT INTO minutes(wall_ms,json) VALUES(?1,?2)", rusqlite::params![minute.last.wall_ms,serde_json::to_string(&minute).unwrap()]).unwrap();
        }
        let query = Query { view: View::History, from_ms: Some(from), to_ms: Some(to), ..Default::default() };
        let report = fixture.query(query.clone());
        assert_eq!(report.coverage.resolution, "minute");
        assert_eq!(report.points.len(), 3);
        assert_eq!(report.coverage.boundary_points, 2);
        assert_eq!(report.points.iter().map(|p| p.overlaps_range_boundary).collect::<Vec<_>>(), [true, false, true]);
        assert_eq!(report.points[0].from_ms, from-30_000, "never clip a summary into false precision");
        assert_eq!(report.points[2].to_ms, to+25_000);
        assert_eq!(report.points[2].peaks.cpu_percent, Some(100.0), "retain the possibly in-range peak");
        assert_eq!(report.points[2].observations, 12, "count describes the whole interval, not a guessed subrange");
        assert!(report.coverage.notes.iter().any(|n| n.contains("peaks may lie outside the requested range")));
        assert!(report.episodes.is_empty(), "minute extrema cannot establish sustained pressure");
        assert_eq!(fixture.store.minutes(from, to, 1).unwrap()[0].last.wall_ms, to+25_000, "overlap filtering precedes the row limit");
        let compacted = fixture.query(Query { limit: 1, ..query });
        assert_eq!(compacted.points.len(), 1);
        assert!(compacted.points[0].overlaps_range_boundary);
        assert_eq!(compacted.coverage.boundary_points, 2);
        assert_eq!(compacted.points[0].peaks.cpu_percent, Some(100.0));
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
        use nitely_core::performance_attribution::{Context, Coverage, Identity, Usage, WorkloadUsage};
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
        assert_eq!(app.related_issues["APP-1"], nitely_core::links::issue_url("APP-1"));
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
    #[test]
    fn before_restart_isolates_boot_identity_when_wall_times_overlap() {
        use nitely_core::performance_attribution::{Context, Coverage, Identity, Usage};
        use nitely_core::performance_events::{self, Event, Kind};
        let mut fixture = Fixture::new();
        for (boot, session, offset, cpu) in [("a", "old", 90_000, 95.0), ("b", "new", 150_000, 10.0)] {
            fixture.store.start_session(session, Some(boot), NOW-700_000).unwrap();
            let mut last = None;
            for n in 0..4 { last = Some(fixture.sample(NOW-offset+n*5000, Some(boot), session, Some(cpu))); }
            let last = last.unwrap();
            let minute = Minute { first_wall_ms: last.wall_ms, first_monotonic_ms: last.monotonic_ms, peaks: last.metrics.clone(), last: last.clone(), samples: 1, min_available_bytes: None };
            fixture.store.db.execute("INSERT INTO minutes(wall_ms,json) VALUES(?1,?2)", rusqlite::params![last.wall_ms,serde_json::to_string(&minute).unwrap()]).unwrap();
            let workload = Workload { id: boot.into(), kind: "build".into(), context: Context { project: Some("APP".into()), ..Default::default() }, root: Identity { boot: boot.into(), pid: 1, started: "1".into() }, start_ms: NOW-600_000, end_ms: Some(last.wall_ms), end_observed: false, job_accounting: None };
            fixture.store.db.execute("INSERT INTO workloads(id,json,ended) VALUES(?1,?2,?3)", rusqlite::params![workload.id,serde_json::to_string(&workload).unwrap(),workload.end_ms]).unwrap();
            let mut attributed = AttributionSample { wall_ms: last.wall_ms, monotonic_ms: last.monotonic_ms, session: session.into(), boot: Some(boot.into()), interval_ms: Some(5000), collection_ms: 1, coverage: Coverage::default(), resolution: "sample".into(), observations: 1, nitely: Usage { cpu_percent: Some(cpu), ..Default::default() }, related: Usage::default(), other: Usage::default(), unattributed_cpu_percent: None, workloads: vec![], top_other: vec![] };
            fixture.store.append_attribution(&mut attributed, true).unwrap();
        }
        for (run, boot) in [(1, Some("a")), (2, Some("b")), (3, None)] {
            performance_events::record(&fixture.dir, &Event { wall_ms: NOW-80_000, boot: boot.map(str::to_string), kind: Kind::Started, project: "APP".into(), issue: None, run, memory_cap_bytes: None, trim: None }).unwrap();
        }
        let query = Query { view: View::History, before_last_restart: true, limit: 1, ..Default::default() };
        let report = fixture.query(query.clone());
        assert_eq!(report.range_boot.as_deref(), Some("a"));
        assert_eq!(report.points.len(), 1);
        assert_eq!(report.points[0].observations, 4);
        assert_eq!(report.points[0].values.cpu_percent, Some(95.0));
        assert_eq!(report.episodes.len(), 1);
        assert_eq!(report.episodes[0].observations, 4);
        assert_eq!(report.attribution.len(), 1, "boot filtering must precede response limits");
        assert_eq!(report.attribution[0].boot.as_deref(), Some("a"));
        assert_eq!(report.workloads.iter().map(|w| w.root.boot.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(report.sessions.iter().map(|s| s.boot.as_deref()).collect::<Vec<_>>(), [Some("a")]);
        assert_eq!(report.events.iter().map(|e| e.run).collect::<Vec<_>>(), [1]);
        assert!(report.coverage.notes.iter().any(|n| n.contains("Events without a recorded boot identity are omitted")));
        assert_eq!(report.latest.unwrap().boot.as_deref(), Some("b"));
        assert_eq!(report.attribution_latest.unwrap().boot.as_deref(), Some("b"));
        fixture.store.db.execute("DELETE FROM samples WHERE json_extract(json,'$.boot')='a'", []).unwrap();
        let minute_report = fixture.query(query);
        assert_eq!(minute_report.coverage.resolution, "minute");
        assert_eq!(minute_report.points.len(), 1);
        assert_eq!(minute_report.points[0].values.cpu_percent, Some(95.0));
        assert!(minute_report.episodes.is_empty(), "minute peaks cannot imply sustained pressure");
    }
}
