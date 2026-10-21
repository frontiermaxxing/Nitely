//! Nitely sizes itself to the machine it runs on (NIT-519). Every few minutes, and before it admits
//! heavy work, it measures what a trivial process takes to start and how much memory is free, and
//! sets how hard builds and tests may push from that: on a machine where starting a process has
//! become slow, test suites that start many processes slow each other down far more than their
//! number, so each gets fewer test threads, fewer full suites run at once, and full suites go to
//! CI where the playbook allows. It returns to normal once starts are fast again. Each change is
//! recorded with its reason, for the Performance page.
//!
//! `pace_for` decides, over plain numbers; the rest measures, caches and records.

use crate::machine::{self, Limits, Memory};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// How fast this machine starts a process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Starts {
    #[default]
    Normal,
    Slow,
    VerySlow,
}

/// A start at or over this (median, milliseconds) is slow. A healthy Windows machine starts
/// `whoami` in about 25 ms; one whose process creation had degraded took 330.
const SLOW_MS: f64 = 80.0;
/// Back to normal only under this, so a machine near the line does not flap.
const SLOW_LEAVE_MS: f64 = 60.0;
const VERY_SLOW_MS: f64 = 250.0;
const VERY_SLOW_LEAVE_MS: f64 = 180.0;
/// What a healthy machine takes, to say how far off this one is.
pub const HEALTHY_START_MS: f64 = 25.0;

/// Test threads and full suites at once while starts are slow, and very slow.
const SLOW_TEST_THREADS: u32 = 8;
const SLOW_SUITES: u32 = 2;
const VERY_SLOW_TEST_THREADS: u32 = 4;
const VERY_SLOW_SUITES: u32 = 1;

/// What the machine measured.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Measured {
    /// Median milliseconds to start a trivial process and see it end; None when it could not be
    /// measured.
    pub start_ms: Option<f64>,
    pub memory: Option<Memory>,
}

/// How hard builds and tests may push now, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pace {
    pub starts: Starts,
    pub start_ms: Option<f64>,
    /// Less memory is free than the floor and one run's cap: admission goes by what running runs
    /// use now (their reservations follow live use, NIT-516), not by what they were admitted with.
    pub memory_short: bool,
    pub available: Option<u64>,
    /// The most test threads one suite gets; None leaves it to the suite's share of cores.
    pub test_threads: Option<u32>,
    /// The most heavy runs (full suites, builds) at once; None leaves it to the machine's slots.
    pub suites: Option<u32>,
    /// Full suites should go to CI when the playbook names them in `ci_commands`.
    pub prefer_ci: bool,
    /// Why, in words a person reads on the Performance page; empty when nothing is held back.
    pub reasons: Vec<String>,
}

/// How fast starts are at `ms`, given how they were (`was`): a level is entered at its threshold
/// and left only under a lower one.
pub fn starts_at(ms: Option<f64>, was: Starts) -> Starts {
    let Some(ms) = ms.filter(|m| m.is_finite()) else { return was };
    match was {
        _ if ms >= VERY_SLOW_MS => Starts::VerySlow,
        Starts::VerySlow if ms >= VERY_SLOW_LEAVE_MS => Starts::VerySlow,
        _ if ms >= SLOW_MS => Starts::Slow,
        Starts::Slow | Starts::VerySlow if ms >= SLOW_LEAVE_MS => Starts::Slow,
        _ => Starts::Normal,
    }
}

/// The pace for what was `measured`, against the machine's memory `limits` and the pace it had
/// (`was`, for the thresholds' hysteresis).
pub fn pace_for(measured: &Measured, limits: Option<&Limits>, was: &Pace) -> Pace {
    let starts = starts_at(measured.start_ms, was.starts);
    let ms = measured.start_ms.or(was.start_ms);
    let mut reasons = Vec::new();
    let (test_threads, suites) = match starts {
        Starts::Normal => (None, None),
        Starts::Slow => (Some(SLOW_TEST_THREADS), Some(SLOW_SUITES)),
        Starts::VerySlow => (Some(VERY_SLOW_TEST_THREADS), Some(VERY_SLOW_SUITES)),
    };
    if let (Some(t), Some(s), Some(ms)) = (test_threads, suites, ms) {
        reasons.push(format!(
            "Starting a process takes {ms:.0} ms here, about {:.0} times a healthy machine's {HEALTHY_START_MS:.0} ms. Test suites start many processes, and several at once slow each other down far more than their number, so each suite gets at most {t} test threads, at most {s} heavy {} at once, and full suites go to CI where the playbook allows.",
            ms / HEALTHY_START_MS,
            if s == 1 { "run" } else { "runs" },
        ));
    }
    // Short with less free than the floor and one run's cap; no longer short once a quarter of a
    // cap more is free again.
    let memory_short = match (measured.memory, limits.filter(|l| l.floor > 0)) {
        (Some(m), Some(l)) => {
            let line = l.floor.saturating_add(l.cap);
            if was.memory_short { m.available < line.saturating_add(l.cap / 4) } else { m.available < line }
        }
        _ => false,
    };
    if memory_short {
        if let (Some(m), Some(l)) = (measured.memory, limits) {
            reasons.push(format!(
                "{} of memory is free, under the {} kept free plus one run's {}: new runs start by the memory the running ones use now, not what they set aside when they started.",
                machine::gb(m.available),
                machine::gb(l.floor),
                machine::gb(l.cap)
            ));
        }
    }
    Pace { starts, start_ms: ms, memory_short, available: measured.memory.map(|m| m.available), test_threads, suites, prefer_ci: starts != Starts::Normal, reasons }
}

impl Pace {
    /// Whether going from `self` to `next` is an adjustment worth recording: what is held back
    /// changed, not merely the numbers measured.
    pub fn changes(&self, next: &Pace) -> bool {
        self.starts != next.starts || self.memory_short != next.memory_short
    }

    /// Test threads for a suite whose share of cores is `jobs`.
    pub fn test_threads_for(&self, jobs: u32) -> u32 {
        self.test_threads.map_or(jobs, |t| t.min(jobs)).max(1)
    }

    /// Slots the machine decided on (`slots`), held to the heavy runs allowed at once.
    pub fn slots_for(&self, slots: u32) -> u32 {
        self.suites.map_or(slots, |s| s.min(slots)).max(1)
    }
}

/// `command` with each `--test-threads N` (or `=N`) over `threads` lowered to it: a playbook's gate
/// that asks for 12 gets fewer while the machine starts processes slowly.
pub fn cap_test_threads(command: &str, threads: u32) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"--test-threads([= ])(\d+)").unwrap());
    re.replace_all(command, |c: &regex::Captures| {
        let n: u32 = c[2].parse().unwrap_or(threads);
        format!("--test-threads{}{}", &c[1], n.min(threads).max(1))
    })
    .into_owned()
}

/// What is said when starts or memory return to normal.
fn recovered(was: &Pace, now: &Pace) -> Vec<String> {
    let mut said = Vec::new();
    if was.starts != Starts::Normal && now.starts == Starts::Normal {
        said.push(format!(
            "Starting a process takes {} again: test threads and heavy runs at once are back to the machine's share.",
            now.start_ms.map_or("normal time".to_string(), |ms| format!("{ms:.0} ms"))
        ));
    }
    if was.memory_short && !now.memory_short {
        said.push(format!("{} of memory is free again: memory is no longer short.", now.available.map_or("Enough".to_string(), machine::gb)));
    }
    said
}

// ----- measuring, the cached pace, and the record -----

/// How often the pace is measured again.
const MEASURE_EVERY: Duration = Duration::from_secs(180);
/// Starts timed per measurement; the median is kept.
const SAMPLES: usize = 5;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cached {
    at_ms: i64,
    pace: Pace,
}

fn pace_file(dir: &Path) -> std::path::PathBuf {
    dir.join("pace.json")
}

fn read_cached(dir: &Path) -> Option<Cached> {
    serde_json::from_str(&std::fs::read_to_string(pace_file(dir)).ok()?).ok()
}

/// The machine's pace now: the cached one, measured again when it is older than a few minutes (by
/// one process at a time; the others keep the cached one meanwhile). Normal where it cannot be
/// told, and always in unit tests, which never measure the machine they run on.
pub fn current() -> Pace {
    if cfg!(test) || std::env::var_os("NITELY_PACE_OFF").is_some() {
        return Pace::default();
    }
    current_in(&crate::paths::data_dir().join("lanes"), &crate::performance::directory())
}

fn current_in(dir: &Path, history: &Path) -> Pace {
    let cached = read_cached(dir);
    let now = crate::performance::wall_ms();
    if let Some(c) = &cached {
        if now.saturating_sub(c.at_ms) < MEASURE_EVERY.as_millis() as i64 {
            return c.pace.clone();
        }
    }
    let was = cached.map(|c| c.pace).unwrap_or_default();
    let Some(_lock) = try_lock(dir) else { return was };
    let measured = Measured { start_ms: crate::procs::start_cost(SAMPLES), memory: machine::memory() };
    let next = pace_for(&measured, machine::limits().as_ref(), &was);
    let cached = Cached { at_ms: now, pace: next.clone() };
    if let Ok(text) = serde_json::to_string(&cached) {
        let tmp = dir.join("pace.json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            std::fs::rename(&tmp, pace_file(dir)).ok();
        }
    }
    if was.changes(&next) {
        record(history, &Adjustment::between(now, &was, &next)).ok();
    }
    next
}

/// The pace last measured, and when, read beside the performance history in `performance_dir`
/// (Nitely's data folder): never measures, so a query of the history starts nothing.
pub fn cached_beside(performance_dir: &Path) -> Option<(i64, Pace)> {
    let c = read_cached(&performance_dir.parent()?.join("lanes"))?;
    Some((c.at_ms, c.pace))
}

fn try_lock(dir: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("pace.lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// One change of pace, as the Performance page shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adjustment {
    pub wall_ms: i64,
    pub pace: Pace,
    /// What changed, in words: the new reasons, or what returned to normal.
    pub said: Vec<String>,
}

impl Adjustment {
    pub fn between(wall_ms: i64, was: &Pace, now: &Pace) -> Adjustment {
        let mut said = recovered(was, now);
        said.extend(now.reasons.iter().cloned());
        Adjustment { wall_ms, pace: now.clone(), said }
    }
}

pub(crate) fn schema(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS pace_adjustment(id INTEGER PRIMARY KEY, wall_ms INTEGER NOT NULL, json TEXT NOT NULL CHECK(length(json)<=8192));
        CREATE INDEX IF NOT EXISTS pace_adjustment_time ON pace_adjustment(wall_ms);",
    )?;
    Ok(())
}

/// Records `adjustment` in the machine's performance history, unless recording is paused. Kept
/// as long as the history's summaries are (the recorder prunes it with them).
pub fn record(dir: &Path, adjustment: &Adjustment) -> Result<()> {
    let mut store = crate::performance::Store::open(dir)?;
    // The paused check and the insert in one write transaction, as workload events do: a pause and
    // clear that comes between them must not see the adjustment come back.
    let tx = store.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("SELECT version FROM recorder_version WHERE id=1", [], |r| r.get(0))?;
    let settings: Option<String> = tx.query_row("SELECT json FROM settings WHERE id=1", [], |r| r.get(0)).optional()?;
    let settings: crate::performance::Settings = settings.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
    if version > crate::performance::RECORDER_VERSION || !settings.enabled {
        return Ok(());
    }
    let oldest = adjustment.wall_ms.saturating_sub(i64::from(settings.summary_days) * 86_400_000);
    tx.execute("DELETE FROM pace_adjustment WHERE wall_ms < ?1", [oldest])?;
    tx.execute("INSERT INTO pace_adjustment(wall_ms, json) VALUES(?1, ?2)", params![adjustment.wall_ms, serde_json::to_string(adjustment)?])?;
    tx.commit()?;
    Ok(())
}

/// The adjustments between `from` and `to`, newest first, at most `limit`.
pub fn adjustments(store: &crate::performance::Store, from: i64, to: i64, limit: usize) -> Result<Vec<Adjustment>> {
    let exists: bool = store.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pace_adjustment')", [], |r| r.get(0))?;
    if !exists {
        return Ok(vec![]);
    }
    let mut stmt = store.db.prepare("SELECT json FROM pace_adjustment WHERE wall_ms >= ?1 AND wall_ms <= ?2 ORDER BY id DESC LIMIT ?3")?;
    let rows = stmt.query_map(params![from, to, limit as i64], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(|r| r.ok().and_then(|j| serde_json::from_str(&j).ok())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::GIB;

    fn at(ms: f64) -> Measured {
        Measured { start_ms: Some(ms), memory: None }
    }

    #[test]
    fn slow_starts_hold_back_test_threads_and_suites_and_recover_with_the_machine() {
        // Today's machine after a reboot, then the same machine before it, then on its way back.
        let healthy = pace_for(&at(23.0), None, &Pace::default());
        assert_eq!((healthy.starts, healthy.test_threads, healthy.suites, healthy.prefer_ci), (Starts::Normal, None, None, false));
        assert!(healthy.reasons.is_empty());
        assert_eq!(healthy.test_threads_for(12), 12);
        assert_eq!(healthy.slots_for(5), 5);

        let degraded = pace_for(&at(330.0), None, &healthy);
        assert_eq!((degraded.starts, degraded.test_threads_for(12), degraded.slots_for(5), degraded.prefer_ci), (Starts::VerySlow, 4, 1, true));
        assert!(healthy.changes(&degraded));
        assert!(degraded.reasons[0].contains("330 ms") && degraded.reasons[0].contains("13 times"), "{:?}", degraded.reasons);
        assert!(!degraded.reasons[0].to_lowercase().contains("reboot"));

        // Better, but not past the line it must cross to leave: no flapping at 200 ms.
        let easing = pace_for(&at(200.0), None, &degraded);
        assert_eq!(easing.starts, Starts::VerySlow);
        let slow = pace_for(&at(120.0), None, &easing);
        assert_eq!((slow.starts, slow.test_threads_for(12), slow.slots_for(5)), (Starts::Slow, 8, 2));
        assert_eq!(pace_for(&at(70.0), None, &slow).starts, Starts::Slow);
        let back = pace_for(&at(30.0), None, &slow);
        assert_eq!(back.starts, Starts::Normal);
        let said = Adjustment::between(1, &slow, &back).said;
        assert!(said[0].contains("30 ms"), "{said:?}");

        // A failed measurement keeps what was.
        assert_eq!(pace_for(&Measured::default(), None, &slow).starts, Starts::Slow);
        // Never more threads than the suite's share of cores.
        assert_eq!(degraded.test_threads_for(2), 2);
    }

    #[test]
    fn memory_is_short_under_the_floor_and_a_cap_and_recovers_past_a_margin() {
        let limits = machine::limits_for(64 * GIB, None, None);
        let free = |gb: u64| Measured { start_ms: Some(23.0), memory: Some(Memory { total: 64 * GIB, available: gb * GIB }) };
        let line = (limits.floor + limits.cap) / GIB; // 12.8 + 16 = 28 GB, rounded down
        let short = pace_for(&free(line - 2), Some(&limits), &Pace::default());
        assert!(short.memory_short && short.reasons.len() == 1);
        assert_eq!((short.test_threads, short.suites), (None, None), "short memory waits in admission; it does not cut threads");
        assert!(pace_for(&free(line + 1), Some(&limits), &short).memory_short, "within a quarter cap of the line it stays short");
        let ok = pace_for(&free(line + 6), Some(&limits), &short);
        assert!(!ok.memory_short && short.changes(&ok));
        // The floor turned off turns this off too.
        let off = machine::limits_for(64 * GIB, Some((0.0, "settings")), None);
        assert!(!pace_for(&free(1), Some(&off), &Pace::default()).memory_short);
    }

    #[test]
    fn a_gate_asking_for_more_test_threads_gets_the_pace() {
        assert_eq!(cap_test_threads("cargo test --workspace -- --test-threads=12", 4), "cargo test --workspace -- --test-threads=4");
        assert_eq!(cap_test_threads("cargo test -- --test-threads 2", 4), "cargo test -- --test-threads 2");
        assert_eq!(cap_test_threads("npm test", 4), "npm test");
    }
}
