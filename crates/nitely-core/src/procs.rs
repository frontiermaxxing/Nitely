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

/// Everything a command starts, held so it can all be ended together, and ended with this process:
/// on Windows a job object that kills its processes when its last handle closes, which is when this
/// guard drops or this process dies; elsewhere nothing (the command runs in its own process group
/// there, and a process's folder can be read to find what it left). A build lane runs each command
/// in one, so nothing a request started outlives it, even when the requester crashes (NIT-223).
pub struct TreeGuard {
    #[cfg(windows)]
    job: win::Job,
}

impl TreeGuard {
    /// A new, empty guard; None when the system will not make one (the command then runs unguarded).
    pub fn new() -> Option<TreeGuard> {
        #[cfg(windows)]
        {
            win::Job::new().map(|job| TreeGuard { job })
        }
        #[cfg(not(windows))]
        {
            Some(TreeGuard {})
        }
    }

    /// Makes `cmd` start suspended (Windows), so `adopt` puts it in the guard before it can start
    /// anything. The command must then be adopted, or it never runs.
    pub fn prepare(&self, cmd: &mut std::process::Command) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
        }
        #[cfg(not(windows))]
        let _ = cmd;
    }

    /// Puts `child`, started by a `prepare`d command, in the guard, then lets it run: everything it
    /// starts is in the guard too. False when it could not be put in (it runs anyway).
    pub fn adopt(&self, child: &std::process::Child) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let added = self.job.assign(child.as_raw_handle());
            win::resume(child.as_raw_handle());
            added
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            true
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
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

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

    /// A job object whose processes end when it is closed.
    pub struct Job(Handle);

    // The handle is only used to assign processes and is closed once, from whichever thread drops it.
    unsafe impl Send for Job {}

    impl Job {
        pub fn new() -> Option<Job> {
            // SAFETY: an unnamed job with default security; the handle is closed in drop.
            let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if job.is_null() {
                return None;
            }
            // SAFETY: all zeros is a valid ExtendedLimits.
            let mut limits: ExtendedLimits = unsafe { std::mem::zeroed() };
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = std::mem::size_of::<ExtendedLimits>() as u32;
            // SAFETY: `limits` is the struct the class names, of `size` bytes.
            let ok = unsafe { SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&mut limits as *mut ExtendedLimits).cast(), size) } != 0;
            if !ok {
                // SAFETY: the job's handle, closed once.
                unsafe { CloseHandle(job) };
                return None;
            }
            Some(Job(job))
        }

        pub fn assign(&self, process: std::os::windows::io::RawHandle) -> bool {
            // SAFETY: both handles are open: the job's own, and a child's process handle.
            unsafe { AssignProcessToJobObject(self.0, process.cast()) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the job's handle, closed once; its processes end with it.
            unsafe { CloseHandle(self.0) };
        }
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(process: Handle, class: u32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
        fn NtResumeProcess(process: Handle) -> i32;
    }

    /// Lets a process started suspended run (all its threads).
    pub fn resume(process: std::os::windows::io::RawHandle) {
        // SAFETY: a child's open process handle.
        unsafe { NtResumeProcess(process.cast()) };
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
