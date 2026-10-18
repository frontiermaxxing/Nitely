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

/// The newer build at this process's path, waiting up to `wait` for an install to put one there;
/// None when none came (or the path cannot be read).
pub fn newer_build(wait: Duration) -> Option<PathBuf> {
    let (path, stamp) = STARTED_AS.get()?.as_ref()?;
    let deadline = Instant::now() + wait;
    loop {
        if nitely_core::install::stamp(path).is_some_and(|now| now != *stamp) {
            return Some(path.clone());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Runs this same command in the newer build at this process's path once one is there, and exits
/// with its status. Returns when no newer build came in `install_wait`.
pub fn hand_over_or_return() {
    let Some(newer) = newer_build(install_wait()) else { return };
    match std::process::Command::new(&newer).args(std::env::args_os().skip(1)).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => eprintln!("starting the newer build at {}: {e}", newer.display()),
    }
}

/// `hand_over_or_return` for a command that read `input` from its stdin: the newer build gets the
/// same line. Whether it ran there.
pub fn hand_over_with_input(input: &str) -> bool {
    use std::io::Write;
    let Some(newer) = newer_build(install_wait()) else { return false };
    let child = std::process::Command::new(&newer).args(std::env::args_os().skip(1)).stdin(std::process::Stdio::piped()).spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("starting the newer build at {}: {e}", newer.display());
            return false;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).ok();
        if !input.ends_with('\n') {
            stdin.write_all(b"\n").ok();
        }
    }
    child.wait().is_ok()
}

/// What a job says when it stopped because its build is older than the database and nothing newer
/// was installed meanwhile.
pub fn older_build(e: &anyhow::Error) -> String {
    let path = STARTED_AS.get().and_then(|s| s.as_ref()).map(|(p, _)| p.display().to_string()).unwrap_or_else(|| "its path".into());
    format!(
        "this Nitely build is older than the database ({e:#}), and no newer build was installed at {path} within {} s",
        install_wait().as_secs()
    )
}
