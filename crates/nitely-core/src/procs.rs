//! Which running processes use a folder: a dev server started in a worktree, a build, a shell
//! whose command `cd`s there. Windows will not delete a folder such a process holds files in, and
//! git then stops halfway through removing a worktree, so land checks this first.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// A process using a folder, by name and pid only: its command line is matched but never kept,
/// since it can carry a token or password.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Proc {
    process_id: u32,
    #[serde(default)]
    parent_process_id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    executable_path: Option<String>,
    #[serde(default)]
    command_line: Option<String>,
    /// The working directory, where the platform tells it (Linux, and macOS through lsof).
    #[serde(skip)]
    cwd: Option<String>,
}

/// Processes whose executable, command line or working directory is inside `dir`, other than this
/// process and the ones that started it. Empty when processes cannot be listed.
pub fn holders(dir: &Path) -> Vec<Holder> {
    holders_of(&[dir]).0.into_iter().next().unwrap_or_default()
}

/// `holders` of each of `dirs`, from one list of the processes, and whether that list is whole: a
/// list that never held this process failed, and says nothing about what runs where.
pub fn holders_of(dirs: &[&Path]) -> (Vec<Vec<Holder>>, bool) {
    // A list without this process failed (a busy machine can time out the query), and taking it
    // for "nothing holds the folder" would let land remove a worktree in use; so ask again.
    let me = std::process::id();
    let mut procs = list();
    for _ in 0..3 {
        if procs.iter().any(|p| p.process_id == me) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        procs = list();
    }
    let parents: HashMap<u32, u32> = procs.iter().map(|p| (p.process_id, p.parent_process_id)).collect();
    let mut mine = vec![me];
    while let Some(&parent) = parents.get(mine.last().unwrap()) {
        if parent == 0 || mine.contains(&parent) {
            break;
        }
        mine.push(parent);
    }
    let whole = procs.iter().any(|p| p.process_id == me);
    let others: Vec<&Proc> = procs.iter().filter(|p| !mine.contains(&p.process_id)).collect();
    let each = dirs
        .iter()
        .map(|dir| {
            let dir = dir.to_string_lossy();
            others
                .iter()
                .filter(|p| [&p.executable_path, &p.command_line, &p.cwd].into_iter().flatten().any(|text| names_path(text, &dir)))
                .map(|p| Holder { pid: p.process_id, name: p.name.clone().unwrap_or_default() })
                .collect()
        })
        .collect();
    (each, whole)
}

/// The command line process `pid` runs, when it is running and the platform lists it. For telling
/// a recorded pid from another process that got the same number since.
pub fn command_line(pid: u32) -> Option<String> {
    list().into_iter().find(|p| p.process_id == pid).and_then(|p| p.command_line)
}

/// Whether any process's command line contains `text` (for tests that start one by a marker).
#[cfg(test)]
pub fn running_with(text: &str) -> bool {
    list().iter().any(|p| p.command_line.as_deref().is_some_and(|c| c.contains(text)))
}

/// Whether process `pid` is running, asked of the system directly: no process is started and no
/// WMI query made, so it is cheap enough to ask as often as a queue looks (NIT-289; it ran
/// `tasklist`, and the queue's polling kept the WMI host busy). A process this user may not look
/// into (another user's, or a protected one) still counts as running.
pub fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        win::alive(pid)
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let Ok(pid) = i32::try_from(pid) else { return false };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 sends nothing; it only asks whether the process exists.
        let r = unsafe { kill(pid, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM: it exists
    }
}

/// When process `pid` started, as a string only to compare: the same pid with another start time is
/// another process that got the number since. None when it is not running or cannot be read.
pub fn started(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        win::started(pid)
    }
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]).output().ok()?;
        // One word ("Fri_Oct_17_09:12:01_2026"), as the pid file keeps it beside the pid.
        let text = String::from_utf8_lossy(&out.stdout).split_whitespace().collect::<Vec<_>>().join("_");
        (out.status.success() && !text.is_empty()).then_some(text)
    }
}

/// How a guarded tree runs (NIT-266).
#[derive(Debug, Clone, Copy)]
pub struct Contain {
    /// What the whole tree may hold in memory, in bytes. On Windows the job refuses memory past it
    /// and says so; elsewhere the tree is measured every second or two. Either way `over_memory`
    /// then answers true and the caller stops it.
    pub memory_cap: Option<u64>,
    /// Below-normal CPU priority with low I/O and memory priority on Windows, nice 10 elsewhere, so
    /// the person's own apps always come first.
    pub low_priority: bool,
    /// The tree ends when the guard drops (or this process dies). A crew worker, which must outlive
    /// its supervisor, keeps running instead, still held to the cap and priority.
    pub end_with_guard: bool,
}

impl Default for Contain {
    fn default() -> Self {
        Contain { memory_cap: None, low_priority: false, end_with_guard: true }
    }
}

/// Everything a command starts, held so it can all be ended together, and ended with this process:
/// on Windows a job object that kills its processes when its last handle closes, which is when this
/// guard drops or this process dies; elsewhere nothing (the command runs in its own process group
/// there, and a process's folder can be read to find what it left). A build lane runs each command
/// in one, so nothing a request started outlives it, even when the requester crashes (NIT-223).
/// With `Contain`, it also holds the tree to a memory cap and a lower priority (NIT-266).
pub struct TreeGuard {
    #[cfg(windows)]
    job: win::Job,
    contain: Contain,
    #[cfg(not(windows))]
    watch: std::sync::Mutex<unix::Watch>,
}

// Elsewhere the guard holds nothing, but dropping it ends what it guards on every system alike.
#[cfg(not(windows))]
impl Drop for TreeGuard {
    fn drop(&mut self) {}
}

impl TreeGuard {
    /// A new, empty guard; None when the system will not make one (the command then runs unguarded).
    pub fn new() -> Option<TreeGuard> {
        TreeGuard::with(Contain::default())
    }

    /// A guard that also holds what it guards as `contain` says.
    pub fn with(contain: Contain) -> Option<TreeGuard> {
        #[cfg(windows)]
        {
            win::Job::new(&contain).map(|job| TreeGuard { job, contain })
        }
        #[cfg(not(windows))]
        {
            Some(TreeGuard { contain, watch: std::sync::Mutex::new(unix::Watch::default()) })
        }
    }

    /// Makes `cmd` start suspended (Windows), so `adopt` puts it in the guard before it can start
    /// anything. The command must then be adopted, or it never runs.
    pub fn prepare(&self, cmd: &mut std::process::Command) {
        self.prepare_with(cmd, 0);
    }

    /// `prepare`, with more Windows creation flags (`CREATE_NEW_PROCESS_GROUP`, say) for `cmd`.
    pub fn prepare_with(&self, cmd: &mut std::process::Command, flags: u32) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0004 | flags); // CREATE_NO_WINDOW | CREATE_SUSPENDED
        }
        #[cfg(not(windows))]
        {
            let _ = flags;
            if self.contain.low_priority {
                unix::nice(cmd);
            }
        }
    }

    /// Starts `cmd` in the guard (`prepare` and `adopt`). On Windows it first tries to start it
    /// apart from any job this process is in (a review's or a crew worker's), so only this guard's
    /// cap counts for it; where that job does not allow it, inside.
    pub fn spawn(&self, cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
        #[cfg(windows)]
        let child = {
            self.prepare_with(cmd, 0x0100_0000); // CREATE_BREAKAWAY_FROM_JOB
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(5) => {
                    self.prepare(cmd);
                    cmd.spawn()?
                }
                other => other?,
            }
        };
        #[cfg(not(windows))]
        let child = {
            self.prepare(cmd);
            cmd.spawn()?
        };
        self.adopt(&child);
        Ok(child)
    }

    /// Puts `child`, started by a `prepare`d command, in the guard, then lets it run: everything it
    /// starts is in the guard too. False when it could not be put in (it runs anyway).
    pub fn adopt(&self, child: &std::process::Child) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let added = self.job.assign(child.as_raw_handle());
            if self.contain.low_priority {
                // I/O and memory priority are the process's own, which what it starts inherits;
                // the job holds the CPU priority of everything in it.
                win::lower(child.as_raw_handle());
            }
            win::resume(child.as_raw_handle());
            added
        }
        #[cfg(not(windows))]
        {
            if let Ok(mut w) = self.watch.lock() {
                w.root = Some(child.id());
            }
            true
        }
    }

    /// Whether what the guard holds went over its memory cap (always false without one).
    pub fn over_memory(&self) -> bool {
        let Some(cap) = self.contain.memory_cap else { return false };
        #[cfg(windows)]
        {
            let _ = cap;
            self.job.memory_limit_hit()
        }
        #[cfg(not(windows))]
        {
            self.watch.lock().map(|mut w| w.over(cap)).unwrap_or(false)
        }
    }

    /// The most memory what the guard holds has used at once, in bytes, where the system tells it
    /// (on Windows exactly; elsewhere as last measured, only with a cap).
    pub fn peak_memory(&self) -> Option<u64> {
        #[cfg(windows)]
        {
            self.job.peak_memory()
        }
        #[cfg(not(windows))]
        {
            self.watch.lock().ok().map(|w| w.peak).filter(|p| *p > 0)
        }
    }

    /// Ends everything the guard holds now.
    pub fn stop(&self) {
        #[cfg(windows)]
        {
            self.job.terminate();
        }
        #[cfg(not(windows))]
        {
            if let Some(root) = self.watch.lock().ok().and_then(|w| w.root) {
                unix::kill_tree(root);
            }
        }
    }
}

/// Whether `text` names `dir` or a path inside it, in any spelling a process might use:
/// `C:\x\y`, `C:/x/y`, Git Bash's `/c/x/y`, WSL's `/mnt/c/x/y`; case-insensitive on Windows.
pub fn names_path(text: &str, dir: &str) -> bool {
    let texts = [normalize(text), normalize(&spell_out(text))];
    let mut spellings = Vec::new();
    // Long or 8.3 short (`RUNNER~1`): a process may have been given either (NIT-104).
    for dir in [dir.to_string(), crate::paths::long_path(dir), crate::paths::short_path(dir)] {
        let dir = normalize(&dir);
        if spellings.contains(&dir) {
            continue;
        }
        if let Some((drive, rest)) = dir.split_once(":/").filter(|(d, _)| d.len() == 1) {
            spellings.push(format!("/{drive}/{rest}"));
            spellings.push(format!("/mnt/{drive}/{rest}"));
        }
        spellings.push(dir);
    }
    // On a Mac, `/var/…` as a process was given it, for the `/private/var/…` git names.
    if cfg!(target_os = "macos") {
        let linked: Vec<String> = spellings.iter().filter_map(|s| through_link(s)).collect();
        spellings.extend(linked);
    }
    // As written, or with its short names spelled out.
    texts.iter().any(|text| {
        spellings.iter().any(|s| {
            text.match_indices(s.as_str()).any(|(i, _)| {
                let after = text[i + s.len()..].chars().next();
                // A whole path component: not `nit-2` inside `nit-27-x`.
                after.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
            })
        })
    })
}

/// `/private/var/x` as `/var/x`: the macOS link most programs are handed (also `/tmp`, `/etc`).
fn through_link(dir: &str) -> Option<String> {
    let rest = dir.strip_prefix("/private")?;
    ["/var/", "/tmp/", "/etc/"].iter().any(|d| rest.starts_with(d)).then(|| rest.to_string())
}

/// `text` with the 8.3 short names in its paths spelled out, so a path written in any mix of
/// short and long names (`C:\Users\RUNNER~1\AppData\...\nit-3`) compares in its long form (NIT-135).
/// A path runs from a drive letter to the next quote, `;`, `&` or `|`, and within that to the
/// longest part ending at a space that exists; the rest is kept as given.
fn spell_out(text: &str) -> String {
    if !cfg!(windows) || !text.contains('~') {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let drive = i + 2 < bytes.len() && bytes[i].is_ascii_alphabetic() && bytes[i + 1] == b':' && matches!(bytes[i + 2], b'\\' | b'/');
        if !drive || (i > 0 && bytes[i - 1].is_ascii_alphanumeric()) {
            let c = text[i..].chars().next().unwrap_or(' ');
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let end = text[i..].find(['"', '\'', ';', '&', '|', '\r', '\n']).map_or(text.len(), |n| i + n);
        let path = &text[i..end];
        if path.contains('~') {
            // The longest part up to a space that exists is the path (`...\COMMON~1 --watch`);
            // without one, the part that exists of the whole is spelled out.
            let cut = path.char_indices().filter(|(_, c)| c.is_whitespace()).map(|(k, _)| k).chain([path.len()]).rev().find(|&k| Path::new(&path[..k]).exists());
            let cut = cut.unwrap_or(path.len());
            out.push_str(&crate::paths::long_path(&path[..cut]));
            out.push_str(&path[cut..]);
        } else {
            out.push_str(path);
        }
        i = end;
    }
    out
}

/// Forward slashes, one at a time, lowercase on Windows (where paths ignore case).
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = if c == '\\' { '/' } else { c };
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    let out = out.trim_end_matches('/').to_string();
    if cfg!(windows) { out.to_lowercase() } else { out }
}

/// Off Windows, a guard's priority is the command's nice value (what it starts inherits it), and
/// its cap is measured: the resident memory of the command and everything under it, every second
/// or two (NIT-266).
#[cfg(not(windows))]
mod unix {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    extern "C" {
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }

    /// Starts `cmd` at nice 10.
    pub fn nice(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: setpriority is a plain system call, safe between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                setpriority(0, 0, 10); // PRIO_PROCESS, this process
                Ok(())
            });
        }
    }

    #[derive(Default)]
    pub struct Watch {
        pub root: Option<u32>,
        pub peak: u64,
        last: Option<Instant>,
        over: bool,
    }

    impl Watch {
        /// Whether the tree under `root` holds more than `cap` bytes, measured at most every
        /// second (Linux) or two (macOS, where it costs a `ps`).
        pub fn over(&mut self, cap: u64) -> bool {
            let every = if cfg!(target_os = "linux") { Duration::from_secs(1) } else { Duration::from_secs(2) };
            if self.over || self.last.is_some_and(|t| t.elapsed() < every) {
                return self.over;
            }
            self.last = Some(Instant::now());
            let Some(root) = self.root else { return false };
            let used = tree(root).iter().map(|(_, _, rss)| rss).sum::<u64>();
            self.peak = self.peak.max(used);
            self.over = used > cap;
            self.over
        }
    }

    /// (pid, parent, resident bytes) of `root` and every process under it.
    fn tree(root: u32) -> Vec<(u32, u32, u64)> {
        let all = processes();
        let mut children: HashMap<u32, Vec<&(u32, u32, u64)>> = HashMap::new();
        for p in &all {
            children.entry(p.1).or_default().push(p);
        }
        let mut out: Vec<(u32, u32, u64)> = all.iter().filter(|p| p.0 == root).cloned().collect();
        let mut i = 0;
        while i < out.len() {
            let pid = out[i].0;
            for c in children.get(&pid).into_iter().flatten() {
                if !out.iter().any(|o| o.0 == c.0) {
                    out.push(**c);
                }
            }
            i += 1;
        }
        out
    }

    #[cfg(target_os = "linux")]
    fn processes() -> Vec<(u32, u32, u64)> {
        extern "C" {
            fn getpagesize() -> i32;
        }
        // SAFETY: a plain call.
        let page = unsafe { getpagesize() }.max(4096) as u64;
        let Ok(dir) = std::fs::read_dir("/proc") else { return vec![] };
        dir.flatten()
            .filter_map(|e| {
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
                // After the name in parentheses: state, ppid, ..., rss (in pages) 22 fields on.
                let rest: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
                Some((pid, rest.get(1)?.parse().ok()?, rest.get(21)?.parse::<u64>().ok()? * page))
            })
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    fn processes() -> Vec<(u32, u32, u64)> {
        let Ok(out) = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,rss="]).output() else { return vec![] };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                Some((w.next()?.parse().ok()?, w.next()?.parse().ok()?, w.next()?.parse::<u64>().ok()? * 1024))
            })
            .collect()
    }

    /// Kills `root` and everything under it.
    pub fn kill_tree(root: u32) {
        let pids: Vec<String> = tree(root).iter().map(|p| p.0.to_string()).collect();
        if !pids.is_empty() {
            std::process::Command::new("kill").arg("-KILL").args(&pids).output().ok();
        }
    }
}

/// From the Windows API, or from PowerShell when that fails: starting PowerShell for the same list
/// takes a second or two, more on a busy machine, and land asks before every cleanup (NIT-208).
#[cfg(windows)]
fn list() -> Vec<Proc> {
    win::list().unwrap_or_else(list_by_powershell)
}

#[cfg(windows)]
fn list_by_powershell() -> Vec<Proc> {
    let script = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine | ConvertTo-Json -Compress";
    // By its full path when it is where Windows keeps it, so a narrow PATH does not hide it.
    let system = std::env::var_os("SystemRoot").map(|r| std::path::Path::new(&r).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"));
    let program = system.filter(|p| p.is_file()).map_or_else(|| "powershell".to_string(), |p| p.to_string_lossy().into_owned());
    let out = crate::playbook::shell_free_command(&program).args(["-NoProfile", "-NonInteractive", "-Command", script]).output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<Vec<Proc>>(text.trim()).unwrap_or_default()
}

/// What `Win32_Process` gives, read the way it reads it: a snapshot of the processes, then each
/// one's executable and command line, where this user may query it.
#[cfg(windows)]
mod win {
    use super::Proc;
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    /// Longest path Windows takes, in UTF-16 units.
    const MAX_PATH_UNITS: usize = 32_768;

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        priority: i32,
        flags: u32,
        exe_file: [u16; 260],
    }

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn QueryFullProcessImageNameW(process: Handle, flags: u32, name: *mut u16, size: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetProcessTimes(process: Handle, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: u32, info: *mut c_void, length: u32) -> i32;
        fn QueryInformationJobObject(job: Handle, class: u32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn CreateIoCompletionPort(file: Handle, existing: Handle, key: usize, threads: u32) -> Handle;
        fn GetQueuedCompletionStatus(port: Handle, bytes: *mut u32, key: *mut usize, overlapped: *mut *mut c_void, millis: u32) -> i32;
        fn SetProcessInformation(process: Handle, class: u32, info: *mut c_void, size: u32) -> i32;
    }

    const JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION: u32 = 7;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
    const JOB_OBJECT_LIMIT_PRIORITY_CLASS: u32 = 0x20;
    const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x200;
    const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x800;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
    const JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
    const JOB_OBJECT_MSG_JOB_MEMORY_LIMIT: u32 = 10;
    /// `ProcessIoPriority` and its `IoPriorityLow`; `ProcessMemoryPriority` and its below normal.
    const PROCESS_IO_PRIORITY: u32 = 33;
    const IO_PRIORITY_LOW: u32 = 1;
    const PROCESS_MEMORY_PRIORITY: u32 = 0;
    const MEMORY_PRIORITY_BELOW_NORMAL: u32 = 4;

    #[repr(C)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    #[repr(C)]
    struct CompletionPort {
        key: *mut c_void,
        port: Handle,
    }

    /// Whether `pid` is running: a handle to it that has not been signalled (a process's handle is
    /// signalled once it exits), or one this user may not open, which exists all the same.
    pub fn alive(pid: u32) -> bool {
        #[link(name = "kernel32")]
        extern "system" {
            fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
            fn GetLastError() -> u32;
        }
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const WAIT_TIMEOUT: u32 = 0x102;
        const ERROR_ACCESS_DENIED: u32 = 5;
        if pid == 0 {
            return false;
        }
        // SAFETY: a plain call; the handle is closed below.
        let process = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            // SAFETY: a plain call, right after the one that failed.
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        }
        // SAFETY: the handle just opened, waited on for no time, then closed once.
        let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
        // SAFETY: as above.
        unsafe { CloseHandle(process) };
        running
    }

    /// When `pid` started (its creation FILETIME), or None.
    pub fn started(pid: u32) -> Option<String> {
        // SAFETY: a plain call; the handle is closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }
        let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
        // SAFETY: four u64s, each the size of a FILETIME, as the call writes.
        let ok = unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } != 0;
        // SAFETY: the handle from OpenProcess, closed once.
        unsafe { CloseHandle(process) };
        ok.then(|| creation.to_string())
    }

    /// A job object: its processes end when it is closed (unless it was made to let them run on),
    /// held to its memory cap and priority, with a completion port that tells when the cap was hit.
    pub struct Job {
        job: Handle,
        port: Handle,
        hit: std::sync::atomic::AtomicBool,
    }

    // The handles are only used through calls that are safe from any thread, and each is closed
    // once, from whichever thread drops the job.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new(contain: &super::Contain) -> Option<Job> {
            // SAFETY: an unnamed job with default security; the handle is closed in drop.
            let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if job.is_null() {
                return None;
            }
            // SAFETY: all zeros is a valid ExtendedLimits.
            let mut limits: ExtendedLimits = unsafe { std::mem::zeroed() };
            // What it holds may start a lane command of its own in a job apart (TreeGuard::spawn).
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            if contain.end_with_guard {
                limits.basic.limit_flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            }
            if contain.low_priority {
                limits.basic.limit_flags |= JOB_OBJECT_LIMIT_PRIORITY_CLASS;
                limits.basic.priority_class = BELOW_NORMAL_PRIORITY_CLASS;
            }
            if let Some(cap) = contain.memory_cap {
                limits.basic.limit_flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                limits.job_memory_limit = cap.min(usize::MAX as u64) as usize;
            }
            let size = std::mem::size_of::<ExtendedLimits>() as u32;
            // SAFETY: `limits` is the struct the class names, of `size` bytes.
            let ok = unsafe { SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&mut limits as *mut ExtendedLimits).cast(), size) } != 0;
            if !ok {
                // SAFETY: the job's handle, closed once.
                unsafe { CloseHandle(job) };
                return None;
            }
            // The port the job posts "the memory cap was hit" to; without one, the cap still holds
            // but goes unreported.
            let mut port = std::ptr::null_mut();
            if contain.memory_cap.is_some() {
                // SAFETY: a new port, not tied to a file; closed in drop.
                port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
                if !port.is_null() {
                    let mut info = CompletionPort { key: job, port };
                    // SAFETY: the struct the class names, of its size.
                    let tied = unsafe {
                        SetInformationJobObject(job, JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION, (&mut info as *mut CompletionPort).cast(), std::mem::size_of::<CompletionPort>() as u32)
                    } != 0;
                    if !tied {
                        // SAFETY: the port's handle, closed once.
                        unsafe { CloseHandle(port) };
                        port = std::ptr::null_mut();
                    }
                }
            }
            Some(Job { job, port, hit: std::sync::atomic::AtomicBool::new(false) })
        }

        pub fn assign(&self, process: std::os::windows::io::RawHandle) -> bool {
            // SAFETY: both handles are open: the job's own, and a child's process handle.
            unsafe { AssignProcessToJobObject(self.job, process.cast()) != 0 }
        }

        /// Whether the job has posted that it hit its memory cap (once it has, always true).
        pub fn memory_limit_hit(&self) -> bool {
            use std::sync::atomic::Ordering;
            if self.port.is_null() || self.hit.load(Ordering::Relaxed) {
                return self.hit.load(Ordering::Relaxed);
            }
            loop {
                let (mut message, mut key, mut overlapped) = (0u32, 0usize, std::ptr::null_mut());
                // SAFETY: the port's handle and three out values; 0 ms, so it never blocks.
                let got = unsafe { GetQueuedCompletionStatus(self.port, &mut message, &mut key, &mut overlapped, 0) } != 0;
                if !got {
                    break;
                }
                if message == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT || message == JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT {
                    self.hit.store(true, Ordering::Relaxed);
                }
            }
            self.hit.load(Ordering::Relaxed)
        }

        /// The most memory the job's processes held at once (committed), in bytes.
        pub fn peak_memory(&self) -> Option<u64> {
            // SAFETY: all zeros is a valid ExtendedLimits.
            let mut limits: ExtendedLimits = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<ExtendedLimits>() as u32;
            // SAFETY: `limits` is the struct the class names, of `size` bytes.
            let ok = unsafe { QueryInformationJobObject(self.job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&mut limits as *mut ExtendedLimits).cast(), size, std::ptr::null_mut()) } != 0;
            (ok && limits.peak_job_memory > 0).then_some(limits.peak_job_memory as u64)
        }

        pub fn terminate(&self) {
            // SAFETY: the job's open handle.
            unsafe { TerminateJobObject(self.job, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the job's handle and its port's, each closed once; unless it lets them run on,
            // its processes end with it.
            unsafe {
                CloseHandle(self.job);
                if !self.port.is_null() {
                    CloseHandle(self.port);
                }
            }
        }
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(process: Handle, class: u32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
        fn NtSetInformationProcess(process: Handle, class: u32, info: *mut c_void, length: u32) -> i32;
        fn NtResumeProcess(process: Handle) -> i32;
    }

    /// Lets a process started suspended run (all its threads).
    pub fn resume(process: std::os::windows::io::RawHandle) {
        // SAFETY: a child's open process handle.
        unsafe { NtResumeProcess(process.cast()) };
    }

    /// Low I/O priority and below-normal memory priority for a child (before it runs), so its disk
    /// work queues behind the person's and its pages are the first trimmed. Best effort.
    pub fn lower(process: std::os::windows::io::RawHandle) {
        let (mut io, mut memory) = (IO_PRIORITY_LOW, MEMORY_PRIORITY_BELOW_NORMAL);
        // SAFETY: a child's open process handle and a u32 of the size given, for each call.
        unsafe {
            NtSetInformationProcess(process.cast(), PROCESS_IO_PRIORITY, (&mut io as *mut u32).cast(), 4);
            SetProcessInformation(process.cast(), PROCESS_MEMORY_PRIORITY, (&mut memory as *mut u32).cast(), 4);
        }
    }

    /// A process's CPU priority class and I/O priority (for tests).
    #[cfg(test)]
    pub fn priorities(pid: u32) -> Option<(u32, u32)> {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetPriorityClass(process: Handle) -> u32;
        }
        // SAFETY: a plain call; the handle is closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }
        let mut io = 0u32;
        // SAFETY: the open handle, a u32 out value of the size given; then closed once.
        let (class, read) = unsafe {
            let class = GetPriorityClass(process);
            let read = NtQueryInformationProcess(process, PROCESS_IO_PRIORITY, (&mut io as *mut u32).cast(), 4, std::ptr::null_mut());
            CloseHandle(process);
            (class, read)
        };
        (read >= 0).then_some((class, io))
    }

    /// Every process, or None when the snapshot fails.
    pub fn list() -> Option<Vec<Proc>> {
        // SAFETY: a plain call; the handle is closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return None;
        }
        // SAFETY: all zeros is a valid ProcessEntry32W, and `size` says which layout this is.
        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut path = vec![0u16; MAX_PATH_UNITS];
        let mut procs = Vec::new();
        // SAFETY: `entry` is a ProcessEntry32W with its size set, as both calls need.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            let len = entry.exe_file.iter().position(|&c| c == 0).unwrap_or(entry.exe_file.len());
            let (executable_path, command_line) = details(entry.process_id, &mut path);
            procs.push(Proc {
                process_id: entry.process_id,
                parent_process_id: entry.parent_process_id,
                name: Some(String::from_utf16_lossy(&entry.exe_file[..len])),
                executable_path,
                command_line,
                cwd: None,
            });
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: the snapshot handle from above, closed once.
        unsafe { CloseHandle(snapshot) };
        (!procs.is_empty()).then_some(procs)
    }

    /// The executable and command line of `pid`, each None where it cannot be read (the idle and
    /// system processes, and another user's or an elevated one).
    fn details(pid: u32, path: &mut [u16]) -> (Option<String>, Option<String>) {
        if pid == 0 {
            return (None, None);
        }
        // SAFETY: a plain call; the handle is closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return (None, None);
        }
        let mut size = path.len() as u32;
        // SAFETY: `path` holds `size` u16s; on success `size` is the length written.
        let exe = (unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut size) } != 0)
            .then(|| String::from_utf16_lossy(&path[..(size as usize).min(path.len())]));
        let command_line = command_line(process);
        // SAFETY: the handle from OpenProcess, closed once.
        unsafe { CloseHandle(process) };
        (exe, command_line)
    }

    /// The command line as the process was started with, which Windows copies into `buf` after a
    /// UNICODE_STRING that points at it.
    fn command_line(process: Handle) -> Option<String> {
        // u64s, so the UNICODE_STRING at its start is aligned.
        let mut buf = vec![0u64; 512];
        for _ in 0..3 {
            let bytes = buf.len() * 8;
            let mut needed = 0u32;
            // SAFETY: `buf` holds `bytes` writable bytes, as the call is told.
            let status = unsafe { NtQueryInformationProcess(process, PROCESS_COMMAND_LINE_INFORMATION, buf.as_mut_ptr().cast(), bytes as u32, &mut needed) };
            if status < 0 {
                // Too small (it says how much it needs), or not allowed.
                if needed as usize > bytes {
                    buf = vec![0u64; (needed as usize).div_ceil(8)];
                    continue;
                }
                return None;
            }
            // SAFETY: on success `buf` starts with a UNICODE_STRING.
            let s = unsafe { &*buf.as_ptr().cast::<UnicodeString>() };
            let (start, end) = (buf.as_ptr() as usize, buf.as_ptr() as usize + bytes);
            let text = s.buffer as usize;
            // Read only what lies inside `buf`.
            if s.buffer.is_null() || text < start || text + s.length as usize > end {
                return None;
            }
            // SAFETY: checked above to lie inside `buf`, which outlives the slice.
            let units = unsafe { std::slice::from_raw_parts(s.buffer, s.length as usize / 2) };
            return Some(String::from_utf16_lossy(units));
        }
        None
    }
}

#[cfg(not(windows))]
fn list() -> Vec<Proc> {
    let Ok(out) = std::process::Command::new("ps").args(["-axo", "pid=,ppid=,comm=,args="]).output() else { return Vec::new() };
    let dirs = working_dirs();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let ppid: u32 = parts.next()?.parse().ok()?;
            let name = parts.next()?.to_string();
            let args = parts.collect::<Vec<_>>().join(" ");
            let cwd = dirs.get(&pid).cloned().or_else(|| std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned()));
            Some(Proc { process_id: pid, parent_process_id: ppid, name: Some(name), executable_path: None, command_line: Some(args), cwd })
        })
        .collect()
}

/// Each process's working directory where there is no /proc to read it from (macOS): one `lsof`
/// call for them all. Processes of other users are left out, as the system does.
#[cfg(target_os = "macos")]
fn working_dirs() -> HashMap<u32, String> {
    let Ok(out) = std::process::Command::new("lsof").args(["-n", "-P", "-w", "-d", "cwd", "-F", "pn"]).output() else { return HashMap::new() };
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn working_dirs() -> HashMap<u32, String> {
    HashMap::new()
}

/// `lsof -F pn` output: a `p<pid>` line, then the `n<path>` of its files.
#[cfg(any(target_os = "macos", test))]
fn parse_lsof(text: &str) -> HashMap<u32, String> {
    let mut dirs = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(path), Some(p)) = (line.strip_prefix('n'), pid) {
            dirs.insert(p, path.to_string());
        }
    }
    dirs
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Liveness from the system itself (NIT-289): this process is running, a child runs until it
    /// exits, and a pid no process has is not.
    #[test]
    fn a_process_is_alive_until_it_exits() {
        assert!(alive(std::process::id()));
        let mut child = if cfg!(windows) {
            crate::playbook::shell_free_command("cmd").args(["/C", "ping -n 30 127.0.0.1 >nul"]).spawn().unwrap()
        } else {
            std::process::Command::new("sleep").arg("30").spawn().unwrap()
        };
        assert!(alive(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!alive(child.id()), "a process that exited (and was waited for) is gone");
        assert!(!alive(0));
        assert!(!alive(u32::MAX - 2), "no such pid");
        if cfg!(windows) {
            assert!(alive(4), "the System process, which this user may not open, still runs");
        } else {
            assert!(alive(1), "init, another user's, still runs");
        }
    }

    /// What a contained command starts runs below normal too, CPU and I/O alike: the job holds the
    /// priority class, and the low I/O priority given to the command passes to its children
    /// (NIT-266).
    #[cfg(windows)]
    #[test]
    fn what_a_low_priority_guard_starts_runs_below_normal() {
        let guard = TreeGuard::with(Contain { memory_cap: Some(1 << 30), low_priority: true, end_with_guard: true }).unwrap();
        // A marker no other process carries: an address only this test pings.
        let mut cmd = crate::playbook::shell_command("ping -n 20 127.0.0.98 >NUL");
        let mut child = guard.spawn(&mut cmd).unwrap();
        let ping = (0..50).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            win::list()?.into_iter().find(|p| p.parent_process_id == child.id() && p.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case("PING.EXE")))
        });
        let ping = ping.expect("the command's ping started");
        // BELOW_NORMAL_PRIORITY_CLASS, IoPriorityLow; the shell itself the same.
        assert_eq!(win::priorities(ping.process_id), Some((0x4000, 1)), "the ping it started");
        assert_eq!(win::priorities(child.id()), Some((0x4000, 1)), "the command");
        guard.stop();
        child.wait().unwrap();
    }

    #[test]
    fn reads_working_dirs_from_lsof() {
        let dirs = parse_lsof("p12\nfcwd\nn/Users/sam/app\np40\nfcwd\nn/private/var/folders/x y\n");
        assert_eq!(dirs.get(&12).map(String::as_str), Some("/Users/sam/app"));
        assert_eq!(dirs.get(&40).map(String::as_str), Some("/private/var/folders/x y"));
        assert_eq!(dirs.len(), 2);
        assert_eq!(through_link("/private/var/folders/x/wt").as_deref(), Some("/var/folders/x/wt"));
        assert_eq!(through_link("/private/stuff/x"), None);
        assert_eq!(through_link("/Users/sam/app"), None);
    }

    #[test]
    fn names_a_folder_in_every_spelling() {
        let dir = if cfg!(windows) { r"C:\Code\Nitely-worktrees\nit-27-lanes" } else { "/c/Code/Nitely-worktrees/nit-27-lanes" };
        assert!(names_path("bash -c 'cd /c/Code/Nitely-worktrees/nit-27-lanes && npm run dev'", dir));
        assert!(!names_path("cd /c/Code/Nitely-worktrees/nit-27-lanes-two && npm run dev", dir), "another worktree");
        assert!(!names_path("cd /c/Code/Nitely && cargo test", dir));
        if cfg!(windows) {
            assert!(names_path(r#""node" "C:\Code\Nitely-worktrees\nit-27-lanes\node_modules\.bin\\..\vite\bin\vite.js" --port 5184"#, dir));
            assert!(names_path("wsl -- ls /mnt/c/code/nitely-worktrees/nit-27-lanes/src", dir));
            assert!(names_path("C:/CODE/Nitely-worktrees/nit-27-lanes", dir));
        }
    }

    #[test]
    fn names_a_folder_written_in_a_mix_of_short_and_long_names() {
        // As on the CI runner: the user folder short (RUNNER~1), the folders below it long, and land
        // asking about git's fully long path (NIT-135). Program Files has a short name where the
        // volume makes them; new folders here get none.
        let Some(program_files) = std::env::var_os("ProgramFiles").map(|p| p.to_string_lossy().into_owned()) else { return };
        let short = crate::paths::short_path(&program_files);
        if !cfg!(windows) || short == program_files {
            return;
        }
        let long = format!(r"{program_files}\Common Files\nit-3-x");
        assert!(names_path(&format!(r#"sh -c "sleep 30; : {short}\Common Files\nit-3-x""#), &long));
        assert!(names_path(&format!(r"node {short}\Common Files\nit-3-x\vite.js --port 5184"), &long));
        assert!(names_path(&format!("cd {}/Common Files/nit-3-x && npm run dev", short.replace('\\', "/")), &long));
        assert!(!names_path(&format!(r"cd {short}\Common Files\nit-3-xy"), &long), "another folder");
        // A short last folder followed by arguments, matched as written or spelled out.
        let common = format!(r"{program_files}\Common Files");
        let common_short = crate::paths::short_path(&common);
        assert!(names_path(&format!("node {common_short} --watch"), &common), "{common_short}");
        assert!(names_path(&format!(r"node {short}\Common Files --watch"), &common_short));
    }

    /// `holders(dir)` once it names `pid`, or as it last was after a few seconds.
    pub(crate) fn held_by(dir: &Path, pid: u32) -> Vec<Holder> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let found = holders(dir);
            if found.iter().any(|h| h.pid == pid) || std::time::Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// The Windows API list reads what PowerShell's Win32_Process does, for this process and the
    /// ones that started it.
    #[cfg(windows)]
    #[test]
    fn the_api_list_matches_win32_process() {
        let native = win::list().expect("a process snapshot");
        let by_powershell = list_by_powershell();
        let me = std::process::id();
        let mine = native.iter().find(|p| p.process_id == me).expect("lists this process");
        let theirs = by_powershell.iter().find(|p| p.process_id == me).expect("PowerShell lists this process");
        assert_eq!(mine.parent_process_id, theirs.parent_process_id);
        assert_eq!(mine.name.as_deref().map(str::to_lowercase), theirs.name.as_deref().map(str::to_lowercase));
        assert_eq!(mine.executable_path, theirs.executable_path);
        assert_eq!(mine.command_line, theirs.command_line);
        assert!(mine.command_line.as_deref().is_some_and(|c| c.contains("nitely_core")), "{mine:?}");
    }

    #[test]
    fn finds_a_process_that_names_the_folder() {
        let dir = crate::paths::unique_temp("nitely-held", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().into_owned();
        // A long-running child whose command line names the folder, like a dev server's would.
        let mut child = if cfg!(windows) {
            crate::playbook::shell_free_command("cmd").args(["/C", &format!("ping -n 30 127.0.0.1 >nul & rem {path}")]).spawn().unwrap()
        } else {
            std::process::Command::new("sh").args(["-c", &format!("sleep 30; : {path}")]).spawn().unwrap()
        };
        let found = held_by(&dir, child.id());
        child.kill().ok();
        child.wait().ok();
        assert!(found.iter().any(|h| h.pid == child.id()), "{found:?}");
        assert!(!found.iter().any(|h| h.pid == std::process::id()), "never this process");
        std::fs::remove_dir_all(&dir).ok();
    }
}
