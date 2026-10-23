//! Trimming the person's idle processes while a run waits for memory (NIT-657). On Windows a
//! process's working set can be emptied without admin rights, for the person's own processes:
//! the pages go to the standby list, which the person's programs fault back cheaply when they run
//! again, and the memory is free for the build meanwhile. Elevated, protected and other users'
//! processes refuse it and are skipped. Nothing here asks for admin or enables a privilege.
//!
//! A trim happens only while a run has waited `WAIT_BEFORE` for memory alone, at most once every
//! `EVERY`, and by default only while the person is away (no input for `AWAY_AFTER`). It trims
//! processes that used under `IDLE_CPU` of a core over the last `IDLE_WINDOW`, never the
//! foreground window's process or this one. The harness and tests never trim.
//!
//! macOS and Linux have no unprivileged equivalent (another process's pages out takes
//! CAP_SYS_NICE; purging caches takes root), and their available-memory figures already count
//! reclaimable cache, so there it is not available.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::Db;

/// How long a run waits for memory before a trim may help it.
pub const WAIT_BEFORE: i64 = 120;
/// The least time between two trims.
pub const EVERY: i64 = 600;
/// No input for this long, and the person is away.
pub const AWAY_AFTER: u64 = 600;
/// A process is idle when it used under this share of one core over `IDLE_WINDOW`.
pub const IDLE_CPU: f64 = 0.01;
pub const IDLE_WINDOW: i64 = 300;

/// When the person lets Nitely trim (Settings, This machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    /// Only while the person is away: the default.
    #[default]
    Away,
    /// Whenever a run has waited long enough.
    Always,
}

/// The setting as Settings shows it: the mode, and whether this system can trim at all.
#[derive(Debug, Clone, Serialize)]
pub struct Setting {
    pub mode: Mode,
    pub available: bool,
    /// Why not, where it is not available.
    pub why: Option<String>,
}

const NOT_HERE: &str = "Not available on this system: macOS and Linux let no program page out another without root, and the memory they count as available already includes what could be freed.";

pub fn setting() -> Setting {
    let mode = crate::Config::load_read_only().ok().and_then(|c| c.memory_trim).unwrap_or_default();
    Setting { mode, available: cfg!(windows), why: (!cfg!(windows)).then(|| NOT_HERE.to_string()) }
}

/// Whether a trim is due now for a run that has waited `waited` seconds for memory alone, the last
/// trim `since_last` seconds ago (None: never), with the person `away` or not.
pub fn due(mode: Mode, available: bool, harness: bool, waited: i64, since_last: Option<i64>, away: bool) -> bool {
    available
        && !harness
        && waited >= WAIT_BEFORE
        && since_last.is_none_or(|s| s >= EVERY)
        && match mode {
            Mode::Off => false,
            Mode::Away => away,
            Mode::Always => true,
        }
}

/// Whether a process that used `cpu_100ns` more processor time over `elapsed` seconds is idle: the
/// window is long enough and it stayed under `IDLE_CPU` of one core.
pub fn idle(cpu_100ns: u64, elapsed: i64) -> bool {
    elapsed >= IDLE_WINDOW && (cpu_100ns as f64) < IDLE_CPU * (elapsed as f64) * 10_000_000.0
}

/// A process as the last sample saw it: identity (pid and creation time) and processor time.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub pid: u32,
    pub started: String,
    pub cpu_100ns: u64,
}

/// The processes to trim: in both samples with the same identity, idle between them, and neither
/// the foreground window's process nor one of `own`.
pub fn candidates(before: &[Sample], now: &[Sample], elapsed: i64, foreground: Option<u32>, own: &[u32]) -> Vec<u32> {
    let earlier: HashMap<(u32, &str), u64> = before.iter().map(|s| ((s.pid, s.started.as_str()), s.cpu_100ns)).collect();
    now.iter()
        .filter(|s| Some(s.pid) != foreground && !own.contains(&s.pid) && s.pid > 4)
        .filter(|s| earlier.get(&(s.pid, s.started.as_str())).is_some_and(|b| idle(s.cpu_100ns.saturating_sub(*b), elapsed)))
        .map(|s| s.pid)
        .collect()
}

/// What one trim did, for the performance history and the waiting run's line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trim {
    pub processes: u32,
    pub available_before: u64,
    pub available_after: u64,
}

/// How a trim reads on the waiting run's line, when it freed something.
pub fn note(t: &Trim) -> Option<String> {
    let freed = t.available_after.saturating_sub(t.available_before);
    (freed >= crate::machine::GIB / 10).then(|| {
        format!("Nitely emptied the working memory of {} idle program{}, {:.1} GB freed", t.processes, if t.processes == 1 { "" } else { "s" }, freed as f64 / crate::machine::GIB as f64)
    })
}

#[derive(Default)]
struct State {
    /// When each run began waiting for memory, as this process saw it.
    waiting_since: HashMap<i64, i64>,
    /// The processes at the start of the idle window, and when.
    sample: Option<(i64, Vec<Sample>)>,
    last_trim: Option<i64>,
    /// What the last trim said, per run that waited through it.
    notes: HashMap<i64, String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn harness() -> bool {
    cfg!(test) || std::env::var_os("NITELY_HARNESS").is_some()
}

/// Called on each look of run `id` that memory alone holds back, with why it waits; returns that
/// line, saying so when a trim helped it. Trims when `due`, and records each trim.
pub fn while_waiting(db: &Db, id: i64, why: String) -> String {
    let now = crate::db::now();
    let setting = setting();
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(State::default);
    state.waiting_since.retain(|_, since| now - *since < 86_400);
    let since = *state.waiting_since.entry(id).or_insert(now);
    // Sampling opens every process, so only where a trim could follow.
    if setting.available && !harness() && setting.mode != Mode::Off {
        let fresh = state.sample.as_ref().is_none_or(|(at, _)| now - at >= 2 * IDLE_WINDOW);
        if fresh {
            state.sample = Some((now, os::samples()));
        }
        let elapsed = state.sample.as_ref().map_or(0, |(at, _)| now - at);
        if elapsed >= IDLE_WINDOW && due(setting.mode, true, false, now - since, state.last_trim.map(|t| now - t), os::away()) {
            let before = state.sample.take().map(|(_, s)| s).unwrap_or_default();
            let pids = candidates(&before, &os::samples(), elapsed, os::foreground(), &[std::process::id()]);
            state.last_trim = Some(now);
            if let Some(trim) = os::trim(&pids) {
                crate::performance_events::lane_trim(db, id, trim);
                if let Some(said) = note(&trim) {
                    for run in state.waiting_since.keys() {
                        state.notes.insert(*run, said.clone());
                    }
                }
            }
        }
    }
    match state.notes.get(&id) {
        Some(said) => format!("{why}; {said}"),
        None => why,
    }
}

#[cfg(windows)]
mod os {
    use super::{Sample, Trim};
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_SET_QUOTA: u32 = 0x0100;

    #[repr(C)]
    struct LastInputInfo {
        size: u32,
        time: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn K32EmptyWorkingSet(process: Handle) -> i32;
        fn ProcessIdToSessionId(process_id: u32, session: *mut u32) -> i32;
        fn GetTickCount() -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowThreadProcessId(window: *mut c_void, process_id: *mut u32) -> u32;
        fn GetLastInputInfo(info: *mut LastInputInfo) -> i32;
    }

    /// The processes now, with their processor time.
    pub fn samples() -> Vec<Sample> {
        crate::procs::metrics()
            .processes
            .into_iter()
            .filter_map(|p| Some(Sample { pid: p.pid, started: p.started, cpu_100ns: p.cpu_100ns? }))
            .collect()
    }

    /// The process of the window the person is using.
    pub fn foreground() -> Option<u32> {
        // SAFETY: plain calls; a null window means there is none, and the id is written only.
        let window = unsafe { GetForegroundWindow() };
        if window.is_null() {
            return None;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        (pid != 0).then_some(pid)
    }

    /// Whether the person has given no input for `AWAY_AFTER`.
    pub fn away() -> bool {
        let mut info = LastInputInfo { size: std::mem::size_of::<LastInputInfo>() as u32, time: 0 };
        // SAFETY: a sized LASTINPUTINFO, written by the call.
        if unsafe { GetLastInputInfo(&mut info) } == 0 {
            return false;
        }
        let idle_ms = unsafe { GetTickCount() }.wrapping_sub(info.time);
        u64::from(idle_ms) >= super::AWAY_AFTER * 1000
    }

    /// Empties the working sets of `pids` in this session that let this user do so, and says how
    /// many and what memory was available before and after. None when it could trim none.
    pub fn trim(pids: &[u32]) -> Option<Trim> {
        let mut session = 0;
        // SAFETY: writes this process's session id.
        if unsafe { ProcessIdToSessionId(std::process::id(), &mut session) } == 0 {
            return None;
        }
        let before = crate::machine::memory()?.available;
        let mut trimmed = 0;
        for &pid in pids {
            let mut theirs = u32::MAX;
            if unsafe { ProcessIdToSessionId(pid, &mut theirs) } == 0 || theirs != session {
                continue;
            }
            // An elevated, protected or other user's process refuses this access: it is skipped.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA, 0, pid) };
            if handle.is_null() {
                continue;
            }
            // SAFETY: a handle opened with the access EmptyWorkingSet needs, closed right after.
            if unsafe { K32EmptyWorkingSet(handle) } != 0 {
                trimmed += 1;
            }
            unsafe { CloseHandle(handle) };
        }
        let after = crate::machine::memory()?.available;
        (trimmed > 0).then_some(Trim { processes: trimmed, available_before: before, available_after: after })
    }
}

#[cfg(not(windows))]
mod os {
    use super::{Sample, Trim};
    pub fn samples() -> Vec<Sample> {
        Vec::new()
    }
    pub fn foreground() -> Option<u32> {
        None
    }
    pub fn away() -> bool {
        false
    }
    pub fn trim(_: &[u32]) -> Option<Trim> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIT-657: when a trim is due, which processes it takes, and what the run's line says.
    #[test]
    fn a_trim_comes_only_after_a_long_memory_wait_and_takes_only_idle_background_programs() {
        assert!(!due(Mode::Away, true, false, WAIT_BEFORE - 1, None, true), "not before two minutes");
        assert!(due(Mode::Away, true, false, WAIT_BEFORE, None, true));
        assert!(!due(Mode::Away, true, false, WAIT_BEFORE, None, false), "the default waits for the person to be away");
        assert!(due(Mode::Always, true, false, WAIT_BEFORE, None, false));
        assert!(!due(Mode::Off, true, false, 3600, None, true));
        assert!(!due(Mode::Always, true, false, 3600, Some(EVERY - 1), true), "at most every ten minutes");
        assert!(!due(Mode::Always, false, false, 3600, None, true), "macOS and Linux");
        assert!(!due(Mode::Always, true, true, 3600, None, true), "never under the harness");

        let s = |pid, cpu| Sample { pid, started: "1".into(), cpu_100ns: cpu };
        let before = [s(10, 0), s(11, 0), s(12, 0), s(13, 0), s(14, 0)];
        // Over five minutes: 10 used 1 s (0.3%), 11 used 10 s (3%), 12 is the foreground, 13 is
        // Nitely, 14 was restarted under the same pid, 15 is new.
        let now = [s(10, 10_000_000), s(11, 100_000_000), s(12, 0), s(13, 0), Sample { pid: 14, started: "2".into(), cpu_100ns: 0 }, s(15, 0)];
        assert_eq!(candidates(&before, &now, IDLE_WINDOW, Some(12), &[13]), [10]);
        assert!(candidates(&before, &now, IDLE_WINDOW - 1, None, &[]).is_empty(), "the window is too short to tell");

        let gb = crate::machine::GIB;
        assert_eq!(note(&Trim { processes: 12, available_before: 18 * gb, available_after: 21 * gb }).as_deref(), Some("Nitely emptied the working memory of 12 idle programs, 3.0 GB freed"));
        assert_eq!(note(&Trim { processes: 3, available_before: 18 * gb, available_after: 18 * gb }), None, "said only when it helped");
        assert!(setting().available == cfg!(windows));
    }
}
