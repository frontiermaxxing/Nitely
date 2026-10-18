//! The machine Nitely's builds share (NIT-266): how much memory it has free, read through the OS
//! and never through a shell, the floor below which nothing new starts, and the cap each run is
//! held to. A lane request, a gate outside lanes, a review and a crew worker each wait for
//! `admit` before they start, so a busy machine slows Nitely down instead of going down with it.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

pub const GIB: u64 = 1 << 30;

/// The machine's memory now, in bytes.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Memory {
    pub total: u64,
    /// What a new process can have without the system paging others out.
    pub available: u64,
}

/// The machine's memory now, or None where it cannot be told. Cheap enough to ask every second:
/// one system call on Windows and macOS, one small file on Linux.
pub fn memory() -> Option<Memory> {
    os::memory()
}

/// What `admit` holds runs to.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Limits {
    pub total: u64,
    /// Nothing new starts while less memory than this is free.
    pub floor: u64,
    /// What one run and everything it starts may hold; a run over it is stopped.
    pub cap: u64,
    /// Where the floor and the cap come from: `default`, `settings` or the environment variable.
    pub floor_from: String,
    pub cap_from: String,
}

/// The limits for a machine with `total` bytes, unless `floor_gb` or `cap_gb` (with where each came
/// from) set them: by default the floor is 20% of memory or 8 GB, whichever is larger, but never
/// more than half; the cap is 25% of memory or 16 GB, whichever is smaller. A floor of 0 turns the
/// wait for memory off.
pub fn limits_for(total: u64, floor_gb: Option<(f64, &str)>, cap_gb: Option<(f64, &str)>) -> Limits {
    let total = total.max(GIB);
    let set = |v: Option<(f64, &str)>| v.filter(|(gb, _)| gb.is_finite() && *gb >= 0.0).map(|(gb, from)| ((gb * GIB as f64) as u64, from.to_string()));
    let (floor, floor_from) = set(floor_gb).unwrap_or_else(|| ((total / 5).max(8 * GIB).min(total / 2), "default".into()));
    let (cap, cap_from) = match set(cap_gb).filter(|(c, _)| *c > 0) {
        Some(c) => c,
        None => ((total / 4).min(16 * GIB), "default".into()),
    };
    Limits { total, floor: floor.min(total), cap: cap.max(GIB / 4), floor_from, cap_from }
}

/// This machine's limits: `NITELY_MEMORY_FLOOR_GB` and `NITELY_MEMORY_CAP_GB`, else the Settings
/// (`memory_floor_gb`, `memory_cap_gb` in Nitely's config), else the defaults. None when the
/// machine's memory cannot be told.
pub fn limits() -> Option<Limits> {
    let total = memory()?.total;
    let env = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<f64>().ok());
    let config = crate::Config::load_read_only().ok();
    let floor = env("NITELY_MEMORY_FLOOR_GB").map(|g| (g, "NITELY_MEMORY_FLOOR_GB")).or(config.as_ref().and_then(|c| c.memory_floor_gb).map(|g| (g, "settings")));
    let cap = env("NITELY_MEMORY_CAP_GB").map(|g| (g, "NITELY_MEMORY_CAP_GB")).or(config.as_ref().and_then(|c| c.memory_cap_gb).map(|g| (g, "settings")));
    Some(limits_for(total, floor, cap))
}

/// How long a run that just started is given to grow before the next one is let in on what is
/// left: a build takes its memory over seconds, not at once.
pub const RAMP: Duration = Duration::from_secs(20);

/// Why a new run must wait, in words ("waiting for memory: …"), or None when it may start now:
/// below the floor nothing starts, and within a cap's room of it, nothing starts until the run
/// that started last has had `RAMP` to grow. A machine whose memory cannot be told never waits.
pub fn admission(now: Option<Memory>, limits: &Limits, since_last_start: Option<Duration>) -> Option<String> {
    let now = now?;
    if limits.floor == 0 {
        return None;
    }
    if now.available < limits.floor {
        return Some(format!("waiting for memory: {} free, new work starts above {}", gb(now.available), gb(limits.floor)));
    }
    if now.available < limits.floor.saturating_add(limits.cap) && since_last_start.is_some_and(|s| s < RAMP) {
        return Some(format!("waiting for memory: {} free, and the last run to start is still growing", gb(now.available)));
    }
    None
}

/// Bytes in GB, as people read them: "6.2 GB".
pub fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GIB as f64)
}

/// The file in `dir` that says when the machine last let a run start.
fn last_start_file(dir: &Path) -> std::path::PathBuf {
    dir.join("last-start")
}

fn since_last_start(dir: &Path) -> Option<Duration> {
    let ms: u128 = std::fs::read_to_string(last_start_file(dir)).ok()?.trim().parse().ok()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    Some(Duration::from_millis(now.saturating_sub(ms) as u64))
}

/// Asks once whether a run may start, under a lock in `dir` (shared by every Nitely process on the
/// machine), and when it may, records the start, so the next process to ask sees it. Returns why it
/// must wait, or None once it may go.
pub fn admit_once(dir: &Path, limits: Option<&Limits>, read: &mut dyn FnMut() -> Option<Memory>) -> Result<Option<String>> {
    let Some(limits) = limits else { return Ok(None) };
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("admission.lock"))?;
    lock.lock()?;
    if let Some(why) = admission(read(), limits, since_last_start(dir)) {
        return Ok(Some(why));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    std::fs::write(last_start_file(dir), now.to_string()).ok();
    Ok(None)
}

/// Waits until a run may start (`admit_once`), telling `waiting` why while it waits and None once it
/// may go. The machine's memory is read every two seconds, and its limits with it (`limits`), so a
/// floor lowered in Settings lets what waits go at its next look.
pub fn admit(dir: &Path, limits: &mut dyn FnMut() -> Option<Limits>, read: &mut dyn FnMut() -> Option<Memory>, waiting: &mut dyn FnMut(Option<&str>)) -> Result<()> {
    let mut said = false;
    loop {
        match admit_once(dir, limits().as_ref(), read)? {
            None => {
                if said {
                    waiting(None);
                }
                return Ok(());
            }
            Some(why) => {
                waiting(Some(&why));
                said = true;
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// `admit` for a process with no queue row to show it on (a review, a crew worker): the machine's
/// own memory and limits, the shared record in Nitely's data folder.
pub fn admit_here(waiting: &mut dyn FnMut(Option<&str>)) -> Result<()> {
    // Unit tests run fake reviewers and workers by the dozen: never held up by the machine they
    // happen to run on, nor recorded in its data folder.
    if cfg!(test) {
        return Ok(());
    }
    admit(&crate::paths::data_dir().join("lanes"), &mut limits, &mut memory, waiting)
}

/// `admit_once` for a process with no queue row (a crew worker about to launch): the machine's own
/// memory and limits, the shared record in Nitely's data folder. Why it must wait, or None once it
/// may go (recorded as a start).
pub fn admit_here_once() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    admit_once(&crate::paths::data_dir().join("lanes"), limits().as_ref(), &mut memory).unwrap_or(None)
}

/// Why a new run would wait now, without waiting or recording anything (what the crew shows).
pub fn would_wait() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let limits = limits()?;
    admission(memory(), &limits, since_last_start(&crate::paths::data_dir().join("lanes")))
}

/// What the strip and `nitely lanes` show about the machine.
#[derive(Debug, Clone, Serialize)]
pub struct Now {
    pub memory: Option<Memory>,
    pub limits: Option<Limits>,
    /// Builds, gates, reviews and crew workers run at below-normal priority (nice 10 off Windows).
    pub below_normal: bool,
}

pub fn now() -> Now {
    Now { memory: memory(), limits: limits(), below_normal: true }
}

#[cfg(windows)]
mod os {
    use super::Memory;

    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    pub fn memory() -> Option<Memory> {
        // SAFETY: all zeros is a valid MemoryStatusEx, and `length` says which layout this is.
        let mut status: MemoryStatusEx = unsafe { std::mem::zeroed() };
        status.length = std::mem::size_of::<MemoryStatusEx>() as u32;
        // SAFETY: a MemoryStatusEx with its length set, as the call needs.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
        (ok && status.total_phys > 0).then_some(Memory { total: status.total_phys, available: status.avail_phys })
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::Memory;

    pub fn memory() -> Option<Memory> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |name: &str| -> Option<u64> { text.lines().find(|l| l.starts_with(name))?.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb * 1024) };
        let total = field("MemTotal:")?;
        Some(Memory { total, available: field("MemAvailable:").or_else(|| field("MemFree:"))? })
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::Memory;
    use std::ffi::{c_char, c_int, c_void, CStr};

    extern "C" {
        fn sysctlbyname(name: *const c_char, old: *mut c_void, old_len: *mut usize, new: *mut c_void, new_len: usize) -> c_int;
    }

    /// An integer sysctl of 4 or 8 bytes.
    fn sysctl(name: &CStr) -> Option<u64> {
        let mut value = [0u8; 8];
        let mut len = value.len();
        // SAFETY: an 8-byte buffer and its length; the call writes at most that.
        let r = unsafe { sysctlbyname(name.as_ptr(), value.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        match (r, len) {
            (0, 8) => Some(u64::from_ne_bytes(value)),
            (0, 4) => Some(u32::from_ne_bytes([value[0], value[1], value[2], value[3]]) as u64),
            _ => None,
        }
    }

    pub fn memory() -> Option<Memory> {
        let total = sysctl(c"hw.memsize")?;
        // The share of memory the system counts as free to use (what Activity Monitor's pressure
        // reads): free, and what it would reclaim without paging anyone out.
        let level = sysctl(c"kern.memorystatus_level")?.min(100);
        Some(Memory { total, available: total / 100 * level })
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod os {
    pub fn memory() -> Option<super::Memory> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(total_gb: u64, free_gb: f64) -> Option<Memory> {
        Some(Memory { total: total_gb * GIB, available: (free_gb * GIB as f64) as u64 })
    }

    #[test]
    fn the_floor_and_cap_come_from_memory_unless_set() {
        let l = limits_for(64 * GIB, None, None);
        assert_eq!((l.floor, l.cap), (64 * GIB / 5, 16 * GIB), "20% and 16 GB");
        let l = limits_for(32 * GIB, None, None);
        assert_eq!((l.floor, l.cap), (8 * GIB, 8 * GIB), "8 GB floor, 25% cap");
        let l = limits_for(8 * GIB, None, None);
        assert_eq!((l.floor, l.cap), (4 * GIB, 2 * GIB), "the floor never passes half");
        let l = limits_for(64 * GIB, Some((4.0, "settings")), Some((10.0, "settings")));
        assert_eq!((l.floor, l.cap, l.floor_from.as_str()), (4 * GIB, 10 * GIB, "settings"));
        assert_eq!(limits_for(64 * GIB, None, Some((0.0, "settings"))).cap, 16 * GIB, "a cap of 0 is not a setting");
        assert_eq!(limits_for(64 * GIB, Some((-1.0, "settings")), None).floor_from, "default");
    }

    #[test]
    fn nothing_starts_below_the_floor_or_while_the_last_start_grows_near_it() {
        let l = limits_for(64 * GIB, None, None); // floor 12.8, cap 16
        let why = admission(mem(64, 6.0), &l, None).unwrap();
        assert!(why.starts_with("waiting for memory: 6.0 GB free"), "{why}");
        assert_eq!(admission(mem(64, 20.0), &l, None), None, "above the floor, nothing started lately");
        assert!(admission(mem(64, 20.0), &l, Some(Duration::from_secs(3))).is_some(), "within a cap of the floor, a fresh start is let grow first");
        assert_eq!(admission(mem(64, 20.0), &l, Some(RAMP + Duration::from_secs(1))), None);
        assert_eq!(admission(mem(64, 40.0), &l, Some(Duration::from_secs(1))), None, "plenty free: no wait at all");
        assert_eq!(admission(None, &l, None), None, "memory that cannot be told never holds work up");
        let off = limits_for(64 * GIB, Some((0.0, "settings")), None);
        assert_eq!(admission(mem(64, 0.5), &off, None), None, "a floor of 0 turns the wait off");
    }

    #[test]
    fn admit_waits_until_memory_frees_and_records_the_start() {
        let dir = crate::paths::unique_temp("nitely-admit", "");
        let l = limits_for(64 * GIB, None, None);
        let mut readings = vec![mem(64, 5.0), mem(64, 9.0), mem(64, 30.0)].into_iter();
        let mut said: Vec<Option<String>> = Vec::new();
        admit(&dir, &mut || Some(l.clone()), &mut || readings.next().flatten(), &mut |w| said.push(w.map(str::to_string))).unwrap();
        assert_eq!(said.len(), 3, "{said:?}");
        assert!(said[0].as_deref().unwrap().starts_with("waiting for memory: 5.0 GB free"));
        assert_eq!(said[2], None, "and says so once it may go");
        assert!(since_last_start(&dir).is_some_and(|s| s < RAMP), "the start is recorded for the next one to ask");
        // The next, with 20 GB free, waits for this one to grow.
        assert!(admit_once(&dir, Some(&l), &mut || mem(64, 20.0)).unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn this_machines_memory_reads_without_a_shell() {
        if let Some(m) = memory() {
            assert!(m.total >= GIB && m.available <= m.total, "{m:?}");
        }
        assert!(cfg!(not(any(windows, target_os = "linux", target_os = "macos"))) || memory().is_some());
    }
}
