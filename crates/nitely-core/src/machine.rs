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
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
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
/// from) set them: by default the floor is 8% of memory, at least 4 GB and at most 8 GB, and never
/// more than half (NIT-656: 20% held builds for hours on a machine whose own programs used the
/// rest); the cap is 25% of memory or 16 GB, whichever is smaller. A floor of 0 turns the wait for
/// memory off.
pub fn limits_for(total: u64, floor_gb: Option<(f64, &str)>, cap_gb: Option<(f64, &str)>) -> Limits {
    let total = total.max(GIB);
    let set = |v: Option<(f64, &str)>| v.filter(|(gb, _)| gb.is_finite() && *gb >= 0.0).map(|(gb, from)| ((gb * GIB as f64) as u64, from.to_string()));
    let (floor, floor_from) = set(floor_gb).unwrap_or_else(|| ((total / 100 * 8).clamp(4 * GIB, 8 * GIB).min(total / 2), "default".into()));
    let (cap, cap_from) = match set(cap_gb).filter(|(c, _)| *c > 0) {
        Some(c) => c,
        None => ((total / 4).min(16 * GIB), "default".into()),
    };
    Limits { total, floor: floor.min(total), cap: cap.max(GIB / 4), floor_from, cap_from }
}

/// This machine's limits: `NITELY_MEMORY_FLOOR_GB` and `NITELY_MEMORY_CAP_GB`, else the Settings
/// (`memory_floor_gb`, `memory_cap_gb` in Nitely's config), else the defaults, with the default
/// floor lowered while the person is away (`away`). None when the machine's memory cannot be told.
pub fn limits() -> Option<Limits> {
    let total = memory()?.total;
    Some(away(limits_for_recorded_total(total), idle()))
}

/// How long without the person's keyboard or mouse before they count as away (NIT-656).
pub const AWAY_AFTER: Duration = Duration::from_secs(15 * 60);

/// The floor while the person is away: room for the system, not for programs they are not using.
pub const AWAY_FLOOR: u64 = 2 * GIB;

/// `limits` for a person idle for `idle`: past `AWAY_AFTER`, a default floor drops to `AWAY_FLOOR`
/// and says it comes from `away`. A floor set in Settings or the environment stays as set, and idle
/// that cannot be told (Linux) keeps the floor.
pub fn away(mut limits: Limits, idle: Option<Duration>) -> Limits {
    if limits.floor_from == "default" && limits.floor > AWAY_FLOOR && idle.is_some_and(|i| i >= AWAY_AFTER) {
        limits.floor = AWAY_FLOOR;
        limits.floor_from = "away".into();
    }
    limits
}

/// How long since the person last used the keyboard or mouse, or None where it cannot be told.
pub fn idle() -> Option<Duration> {
    os::idle()
}

/// How long a run waits for memory with nothing of Nitely's running before it starts on what is
/// free, if its own need fits there (NIT-656).
pub const LONG_WAIT: Duration = Duration::from_secs(10 * 60);

/// What a waiting run knows of its wait (NIT-656): how long it has waited, and whether anything of
/// Nitely's runs now.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Held {
    pub waited: Duration,
    pub nitely_running: bool,
}

impl Held {
    /// Past `LONG_WAIT` with nothing of Nitely's running: what holds it is the machine's own use.
    pub fn long(&self) -> bool {
        self.waited >= LONG_WAIT && !self.nitely_running
    }
}

/// The least a run with no expected need is taken to need when it starts past a long wait.
const LONG_WAIT_MIN_NEED: u64 = GIB;

/// The admission decision whole (NIT-656): `admission_for`, except that a run held past
/// `LONG_WAIT` with nothing of Nitely's running starts once its own need (at least
/// `LONG_WAIT_MIN_NEED`) is free, floor or not: nothing of Nitely's would make room, so waiting on
/// the floor would only wait on the person's own programs. One that does not fit even so says what
/// it needs and what is free. The away floor is already in `limits` (`away`).
pub fn decide(now: Option<Memory>, limits: &Limits, since_last_start: Option<Duration>, need: u64, ramp: bool, held: Held) -> Option<String> {
    let why = admission_for(now, limits, since_last_start, need, ramp)?;
    let (Some(now), true) = (now, held.long()) else { return Some(why) };
    let need = need.max(LONG_WAIT_MIN_NEED);
    if now.available >= need {
        return None;
    }
    Some(format!(
        "waiting for memory for {} min with nothing of Nitely's running: needs {} free, {} free",
        held.waited.as_secs() / 60,
        gb(need),
        gb(now.available)
    ))
}

/// Current policy applied to a cached physical total; does not sample the OS or admit work.
pub fn limits_for_recorded_total(total: u64) -> Limits {
    let env = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<f64>().ok());
    let config = crate::Config::load_read_only().ok();
    let floor = env("NITELY_MEMORY_FLOOR_GB").map(|g| (g, "NITELY_MEMORY_FLOOR_GB")).or(config.as_ref().and_then(|c| c.memory_floor_gb).map(|g| (g, "settings")));
    let cap = env("NITELY_MEMORY_CAP_GB").map(|g| (g, "NITELY_MEMORY_CAP_GB")).or(config.as_ref().and_then(|c| c.memory_cap_gb).map(|g| (g, "settings")));
    limits_for(total, floor, cap)
}

/// How long a run that just started is given to grow before the next one is let in on what is
/// left: a build takes its memory over seconds, not at once.
pub const RAMP: Duration = Duration::from_secs(20);

/// Why a new run must wait, in words ("waiting for memory: …"), or None when it may start now:
/// below the floor nothing starts, and within a cap's room of it, nothing starts until the run
/// that started last has had `RAMP` to grow. A machine whose memory cannot be told never waits.
pub fn admission(now: Option<Memory>, limits: &Limits, since_last_start: Option<Duration>) -> Option<String> {
    admission_for(now, limits, since_last_start, 0, true)
}

/// `admission` for a run expected to take `need` (NIT-363): it starts only while that much is free
/// above the floor too, so taking what it is expected to never brings the machine under the floor.
/// With `ramp` false (a light job, NIT-454), it does not wait out the last start's `RAMP`: what it
/// takes is small and known, and the floor check above covers it.
pub fn admission_for(now: Option<Memory>, limits: &Limits, since_last_start: Option<Duration>, need: u64, ramp: bool) -> Option<String> {
    let now = now?;
    if limits.floor == 0 {
        return None;
    }
    if now.available < limits.floor {
        return Some(format!("waiting for memory: {} free, new work starts above {}", gb(now.available), gb(limits.floor)));
    }
    if now.available < limits.floor.saturating_add(need) {
        return Some(format!(
            "waiting for memory: {} free, and this run is expected to take {} above the {} floor",
            gb(now.available),
            gb(need),
            gb(limits.floor)
        ));
    }
    if ramp && now.available < limits.floor.saturating_add(limits.cap) && since_last_start.is_some_and(|s| s < RAMP) {
        return Some(format!("waiting for memory: {} free, and the last run to start is still growing", gb(now.available)));
    }
    None
}

/// Bytes in GB, as people read them: "6.2 GB".
pub fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GIB as f64)
}

/// Bytes free to this user on the volume holding `path` (its nearest existing ancestor): what the
/// crew reads before it sets up another worktree (NIT-246).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let mut at = path;
    while !at.exists() {
        at = at.parent()?;
    }
    free_disk(at)
}

#[cfg(windows)]
fn free_disk(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: a null-terminated path and three out-values that live through the call.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some(free)
}

#[cfg(unix)]
fn free_disk(path: &Path) -> Option<u64> {
    // POSIX df: "Filesystem 1024-blocks Used Available Capacity Mounted-on", one line per volume.
    let out = std::process::Command::new("df").arg("-Pk").arg(path).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}

/// Why no new worktree should be set up now: less than `min_gb` free where worktrees go. None when
/// there is room, when `min_gb` is 0, or when it cannot be read.
pub fn disk_short(worktrees: &Path, min_gb: u64) -> Option<String> {
    let free = free_disk_bytes(worktrees).filter(|f| min_gb > 0 && *f < min_gb * GIB)?;
    Some(format!("disk low: {} free where worktrees go (crew.min_free_disk_gb is {min_gb} GB)", gb(free)))
}

/// The file in `dir` that says when the machine last let a run start.
fn last_start_file(dir: &Path) -> std::path::PathBuf {
    dir.join("last-start")
}

fn since_last_start(dir: &Path) -> Option<Duration> {
    since_last_start_at(dir, wall_ms())
}

/// How long before `now_ms` the last run started, from the record in `dir`.
fn since_last_start_at(dir: &Path, now_ms: u128) -> Option<Duration> {
    let ms: u128 = std::fs::read_to_string(last_start_file(dir)).ok()?.trim().parse().ok()?;
    Some(Duration::from_millis(now_ms.saturating_sub(ms) as u64))
}

/// Milliseconds since the Unix epoch, as the start record keeps them.
fn wall_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Asks once whether a run may start, under a lock in `dir` (shared by every Nitely process on the
/// machine), and when it may, records the start, so the next process to ask sees it. Returns why it
/// must wait, or None once it may go.
pub fn admit_once(dir: &Path, limits: Option<&Limits>, read: &mut dyn FnMut() -> Option<Memory>) -> Result<Option<String>> {
    admit_once_with(dir, limits, 0, true, Held::default(), read, &mut || Ok(None), &mut || Ok(()))
}

/// `admit_once`, with the caller's own `check` first and its `commit` once the run may go, all
/// under the one lock, so two processes never both take what only one of them may (NIT-323: two
/// requests each finding room for their cap before either recorded it). `held` is what the run
/// knows of its wait (`decide`).
#[allow(clippy::too_many_arguments)]
pub fn admit_once_with(
    dir: &Path,
    limits: Option<&Limits>,
    need: u64,
    ramp: bool,
    held: Held,
    read: &mut dyn FnMut() -> Option<Memory>,
    check: &mut dyn FnMut() -> Result<Option<String>>,
    commit: &mut dyn FnMut() -> Result<()>,
) -> Result<Option<String>> {
    admit_once_at(dir, limits, need, ramp, held, read, check, commit, wall_ms())
}

/// `admit_once_with` at `now_ms` (NIT-533): the start is recorded as then, and the last one is
/// measured from it, so a test can say what time it is.
#[allow(clippy::too_many_arguments)]
pub fn admit_once_at(
    dir: &Path,
    limits: Option<&Limits>,
    need: u64,
    ramp: bool,
    held: Held,
    read: &mut dyn FnMut() -> Option<Memory>,
    check: &mut dyn FnMut() -> Result<Option<String>>,
    commit: &mut dyn FnMut() -> Result<()>,
    now_ms: u128,
) -> Result<Option<String>> {
    let Some(limits) = limits else { return Ok(None) };
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("admission.lock"))?;
    lock.lock()?;
    if let Some(why) = check()? {
        return Ok(Some(why));
    }
    if let Some(why) = decide(read(), limits, since_last_start_at(dir, now_ms), need, ramp, held) {
        return Ok(Some(why));
    }
    commit()?;
    std::fs::write(last_start_file(dir), now_ms.to_string()).ok();
    Ok(None)
}

/// Waits until a run may start (`admit_once`), telling `waiting` why while it waits and None once it
/// may go. The machine's memory is read every two seconds, and its limits with it (`limits`), so a
/// floor lowered in Settings lets what waits go at its next look.
pub fn admit(dir: &Path, limits: &mut dyn FnMut() -> Option<Limits>, read: &mut dyn FnMut() -> Option<Memory>, waiting: &mut dyn FnMut(Option<&str>)) -> Result<()> {
    admit_with(dir, limits, read, waiting, &mut std::thread::sleep, &mut wall_ms)
}

/// `admit` with its pause between looks (`sleep`) and its clock (`now_ms`) given (NIT-533).
pub fn admit_with(
    dir: &Path,
    limits: &mut dyn FnMut() -> Option<Limits>,
    read: &mut dyn FnMut() -> Option<Memory>,
    waiting: &mut dyn FnMut(Option<&str>),
    sleep: &mut dyn FnMut(Duration),
    now_ms: &mut dyn FnMut() -> u128,
) -> Result<()> {
    let mut said = false;
    loop {
        match admit_once_at(dir, limits().as_ref(), 0, true, Held::default(), read, &mut || Ok(None), &mut || Ok(()), now_ms())? {
            None => {
                if said {
                    waiting(None);
                }
                return Ok(());
            }
            Some(why) => {
                waiting(Some(&why));
                said = true;
                sleep(Duration::from_secs(2));
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

/// A reviewer waiting for memory can stop when the person disables review. Propagate policy and
/// admission errors; the caller checks once more immediately before spawning the process.
pub fn admit_here_checked(check: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    loop {
        check()?;
        if cfg!(test) || admit_once(&crate::paths::data_dir().join("lanes"), limits().as_ref(), &mut memory)?.is_none() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
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
    /// The cap one run is held to now: the limits' cap, or less when the machine's slots at that
    /// cap would not fit above the floor (`lanes::cap_for`).
    pub run_cap: Option<u64>,
    /// Builds, gates, reviews and crew workers run at below-normal priority (nice 10 off Windows).
    pub below_normal: bool,
    /// How hard builds and tests may push now, from the machine's measured process starts and
    /// memory, and why (NIT-519).
    pub pace: crate::pace::Pace,
}

pub fn now() -> Now {
    let limits = limits();
    let run_cap = limits.as_ref().map(|l| crate::lanes::cap_for(l, crate::lanes::machine_budget().slots));
    Now { memory: memory(), limits, run_cap, below_normal: true, pace: crate::pace::current() }
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

    #[repr(C)]
    struct LastInputInfo {
        size: u32,
        time: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetLastInputInfo(info: *mut LastInputInfo) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount() -> u32;
    }

    /// Since the last input in this session: both are milliseconds of the same 32-bit tick count,
    /// so the difference holds across its wrap every 49.7 days.
    pub fn idle() -> Option<std::time::Duration> {
        let mut info = LastInputInfo { size: std::mem::size_of::<LastInputInfo>() as u32, time: 0 };
        // SAFETY: a LastInputInfo with its size set, as the call needs.
        let ok = unsafe { GetLastInputInfo(&mut info) } != 0;
        // SAFETY: no arguments.
        let now = unsafe { GetTickCount() };
        ok.then(|| std::time::Duration::from_millis(now.wrapping_sub(info.time) as u64))
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

    /// No one way to tell across X11, Wayland and a console: the floor stays.
    pub fn idle() -> Option<std::time::Duration> {
        None
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

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }

    /// Since the last input of any kind (`kCGAnyInputEventType`) in the login session
    /// (`kCGEventSourceStateCombinedSessionState`).
    pub fn idle() -> Option<std::time::Duration> {
        // SAFETY: two plain values in, a number out.
        let secs = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
        (secs.is_finite() && secs >= 0.0).then(|| std::time::Duration::from_secs_f64(secs))
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod os {
    pub fn memory() -> Option<super::Memory> {
        None
    }

    pub fn idle() -> Option<std::time::Duration> {
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
        assert_eq!((l.floor, l.cap), (64 * GIB / 100 * 8, 16 * GIB), "8% and 16 GB");
        let l = limits_for(32 * GIB, None, None);
        assert_eq!((l.floor, l.cap), (4 * GIB, 8 * GIB), "at least 4 GB, 25% cap");
        assert_eq!(limits_for(256 * GIB, None, None).floor, 8 * GIB, "at most 8 GB");
        let l = limits_for(6 * GIB, None, None);
        assert_eq!((l.floor, l.cap), (3 * GIB, 6 * GIB / 4), "the floor never passes half");
        let l = limits_for(64 * GIB, Some((4.0, "settings")), Some((10.0, "settings")));
        assert_eq!((l.floor, l.cap, l.floor_from.as_str()), (4 * GIB, 10 * GIB, "settings"));
        assert_eq!(limits_for(64 * GIB, None, Some((0.0, "settings"))).cap, 16 * GIB, "a cap of 0 is not a setting");
        assert_eq!(limits_for(64 * GIB, Some((-1.0, "settings")), None).floor_from, "default");
    }

    #[test]
    fn nothing_starts_below_the_floor_or_while_the_last_start_grows_near_it() {
        let l = limits_for(64 * GIB, Some((12.8, "settings")), None); // floor 12.8, cap 16
        let why = admission(mem(64, 6.0), &l, None).unwrap();
        assert!(why.starts_with("waiting for memory: 6.0 GB free"), "{why}");
        assert_eq!(admission(mem(64, 20.0), &l, None), None, "above the floor, nothing started lately");
        assert!(admission(mem(64, 20.0), &l, Some(Duration::from_secs(3))).is_some(), "within a cap of the floor, a fresh start is let grow first");
        assert_eq!(admission(mem(64, 20.0), &l, Some(RAMP + Duration::from_secs(1))), None);
        assert_eq!(admission(mem(64, 40.0), &l, Some(Duration::from_secs(1))), None, "plenty free: no wait at all");
        assert_eq!(admission(None, &l, None), None, "memory that cannot be told never holds work up");
        let off = limits_for(64 * GIB, Some((0.0, "settings")), None);
        assert_eq!(admission(mem(64, 0.5), &off, None), None, "a floor of 0 turns the wait off");
        // A run expected to take 4 GB starts only with that much free above the floor (NIT-363).
        let need = 4 * GIB;
        let why = admission_for(mem(64, 14.0), &l, None, need, true).unwrap();
        assert!(why.contains("expected to take 4.0 GB above the"), "{why}");
        assert_eq!(admission_for(mem(64, 17.0), &l, None, need, true), None);
        // A light job does not wait out the last start's ramp; a heavy one does (NIT-454).
        let fresh = Some(Duration::from_secs(1));
        assert!(admission_for(mem(64, 20.0), &l, fresh, GIB, true).is_some_and(|w| w.contains("still growing")));
        assert_eq!(admission_for(mem(64, 20.0), &l, fresh, GIB, false), None);
    }

    /// NIT-656: the 64 GB machine whose own programs left 15.9 GB free held a build needing 11.9 GB
    /// for hours behind a 12.8 GB floor. Away, the floor is 2 GB; past ten minutes with nothing of
    /// Nitely's running, a run whose need alone fits starts, and one that does not says why.
    #[test]
    fn a_long_wait_or_an_away_person_lets_a_run_that_fits_start() {
        let l = limits_for(64 * GIB, None, None);
        let need = 11 * GIB;
        let fresh = Held { waited: Duration::from_secs(60), nitely_running: false };
        assert!(decide(mem(64, 15.9), &l, None, need, true, fresh).is_some(), "5.1 GB floor + 11 GB: waits at first");
        // Away for 20 minutes: a 2 GB floor; 13 GB fits. Under 15 minutes, or a floor set in
        // Settings, keeps the floor; idle that cannot be told keeps it too.
        let gone = away(l.clone(), Some(Duration::from_secs(20 * 60)));
        assert_eq!((gone.floor, gone.floor_from.as_str()), (AWAY_FLOOR, "away"));
        assert_eq!(decide(mem(64, 13.5), &gone, None, need, true, fresh), None);
        assert_eq!(away(l.clone(), Some(Duration::from_secs(60))), l);
        assert_eq!(away(l.clone(), None), l);
        let set = limits_for(64 * GIB, Some((12.8, "settings")), None);
        assert_eq!(away(set.clone(), Some(Duration::from_secs(3600))), set, "the person's own floor stays");
        // Ten minutes in with nothing of Nitely's running: its need alone decides.
        let long = Held { waited: LONG_WAIT, nitely_running: false };
        assert_eq!(decide(mem(64, 12.0), &set, None, need, true, long), None, "11 GB fits in 12 GB free");
        let why = decide(mem(64, 9.0), &set, None, need, true, long).unwrap();
        assert_eq!(why, "waiting for memory for 10 min with nothing of Nitely's running: needs 11.0 GB free, 9.0 GB free");
        // While a run of Nitely's holds memory, it makes room by ending: the floor holds.
        let busy = Held { waited: LONG_WAIT * 3, nitely_running: true };
        assert!(decide(mem(64, 12.0), &set, None, need, true, busy).is_some());
        // With no expected need, at least 1 GB must be free.
        assert!(decide(mem(64, 0.5), &set, None, 0, true, long).is_some());
        assert_eq!(decide(mem(64, 1.5), &set, None, 0, true, long), None);
    }

    #[test]
    fn admit_waits_until_memory_frees_and_records_the_start() {
        let dir = crate::paths::unique_temp("nitely-admit", "");
        let l = limits_for(64 * GIB, Some((12.8, "settings")), None);
        let mut readings = vec![mem(64, 5.0), mem(64, 9.0), mem(64, 30.0)].into_iter();
        let mut said: Vec<Option<String>> = Vec::new();
        // A clock that moves only when the loop pauses, and pauses that take no time (NIT-533).
        let clock = std::cell::Cell::new(1_000_000u128);
        let mut slept = Vec::new();
        admit_with(
            &dir,
            &mut || Some(l.clone()),
            &mut || readings.next().flatten(),
            &mut |w| said.push(w.map(str::to_string)),
            &mut |d| {
                slept.push(d);
                clock.set(clock.get() + d.as_millis());
            },
            &mut || clock.get(),
        )
        .unwrap();
        assert_eq!(slept, vec![Duration::from_secs(2); 2], "it looked again every two seconds while it waited");
        assert_eq!(said.len(), 3, "{said:?}");
        assert!(said[0].as_deref().unwrap().starts_with("waiting for memory: 5.0 GB free"));
        assert_eq!(said[2], None, "and says so once it may go");
        assert_eq!(since_last_start_at(&dir, clock.get()), Some(Duration::ZERO), "the start is recorded for the next one to ask");
        // The next, with 20 GB free, waits for this one to grow, and goes once it has had the ramp.
        let next = |at: u128| admit_once_at(&dir, Some(&l), 0, true, Held::default(), &mut || mem(64, 20.0), &mut || Ok(None), &mut || Ok(()), at).unwrap();
        assert!(next(clock.get() + 1_000).is_some());
        assert_eq!(next(clock.get() + RAMP.as_millis()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn this_machines_memory_reads_without_a_shell() {
        if let Some(m) = memory() {
            assert!(m.total >= GIB && m.available <= m.total, "{m:?}");
        }
        assert!(cfg!(not(any(windows, target_os = "linux", target_os = "macos"))) || memory().is_some());
    }

    #[test]
    fn reads_free_disk_where_worktrees_will_go_and_says_when_short() {
        let here = std::env::temp_dir();
        assert!(free_disk_bytes(&here.join("not-made-yet").join("deeper")).is_some_and(|b| b > 0), "read on the nearest folder that exists");
        assert_eq!(disk_short(&here, 0), None, "0 is off");
        assert!(disk_short(&here, 1 << 30).unwrap().starts_with("disk low:"), "no volume has a billion GB free");
    }
}
