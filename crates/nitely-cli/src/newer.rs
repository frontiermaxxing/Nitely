//! A background process of nitely's own (a submit job, a triage answer, a detached MCP call) that
//! finds the database upgraded past its build: a newer build upgrades the database a moment before
//! its install puts that build at this process's path, so the work goes to that build once it is
//! there, instead of the process dying with nothing said (NIT-314).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// This process's build as it was at its path when the process started.
static STARTED_AS: OnceLock<Option<(PathBuf, (u64, SystemTime))>> = OnceLock::new();

/// Notes this process's build; called first thing in `main`.
pub fn started() {
    STARTED_AS.get_or_init(|| std::env::current_exe().ok().and_then(|p| nitely_core::install::stamp(&p).map(|s| (p, s))));
}

/// Whether `e` is the database refusing this build as older than it.
pub fn is_newer_database(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<nitely_core::db::NewerDatabase>())
}

/// How long to wait for the newer build's install: `NITELY_MCP_INSTALL_WAIT` (seconds) sets it,
/// for tests, as it does for the MCP server.
pub fn install_wait() -> Duration {
    Duration::from_secs(std::env::var("NITELY_MCP_INSTALL_WAIT").ok().and_then(|s| s.parse().ok()).unwrap_or(90))
}

/// Whether the file at `path` is another build than `was`, and a whole one: an install moves the
/// old file aside and then copies the new one in, so a file still being written is not one yet.
/// It counts once it has held still for a moment (NIT-348).
pub fn replaced(path: &std::path::Path, was: &(u64, SystemTime)) -> bool {
    let Some(now) = nitely_core::install::stamp(path).filter(|s| s != was) else { return false };
    std::thread::sleep(Duration::from_millis(750));
    nitely_core::install::stamp(path) == Some(now)
}

/// The newer build at this process's path, waiting until `deadline` for an install to put one
/// there; None when none came (or the path cannot be read).
fn newer_build(deadline: Instant) -> Option<PathBuf> {
    let (path, stamp) = STARTED_AS.get()?.as_ref()?;
    loop {
        if replaced(path, stamp) {
            return Some(path.clone());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Starts `make()`'s command in the newer build at this process's path once one is there, trying
/// again while it cannot start (an install still finishing) until `install_wait` is up.
fn start_newer(make: impl Fn(&std::path::Path) -> std::process::Command) -> Option<std::process::Child> {
    let deadline = Instant::now() + install_wait();
    loop {
        let newer = newer_build(deadline)?;
        match make(&newer).spawn() {
            Ok(child) => return Some(child),
            Err(e) if Instant::now() >= deadline => {
                eprintln!("starting the newer build at {}: {e}", newer.display());
                return None;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// Runs this same command in the newer build at this process's path once one is there, and exits
/// with its status. Returns when no newer build came, or none could start, in `install_wait`.
pub fn hand_over_or_return() {
    let Some(mut child) = start_newer(|newer| {
        let mut cmd = std::process::Command::new(newer);
        cmd.args(std::env::args_os().skip(1));
        cmd
    }) else {
        return;
    };
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    std::process::exit(code);
}

/// `hand_over_or_return` for a command that read `input` from its stdin: the newer build gets the
/// same line. Whether it ran there.
pub fn hand_over_with_input(input: &str) -> bool {
    use std::io::Write;
    let Some(mut child) = start_newer(|newer| {
        let mut cmd = std::process::Command::new(newer);
        cmd.args(std::env::args_os().skip(1)).stdin(std::process::Stdio::piped());
        cmd
    }) else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).ok();
        if !input.ends_with('\n') {
            stdin.write_all(b"\n").ok();
        }
    }
    child.wait().is_ok()
}

/// What a job says when it stopped because its build is older than the database and no newer build
/// was installed (or could start) meanwhile.
pub fn older_build(e: &anyhow::Error) -> String {
    let path = STARTED_AS.get().and_then(|s| s.as_ref()).map(|(p, _)| p.display().to_string()).unwrap_or_else(|| "its path".into());
    format!(
        "this Nitely build is older than the database ({e:#}), and no newer build was installed at {path} within {} s",
        install_wait().as_secs()
    )
}
