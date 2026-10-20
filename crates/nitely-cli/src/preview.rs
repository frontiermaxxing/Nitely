use anyhow::{bail, Context, Result};
use clap::Subcommand;
use nitely_core::{paths, procs};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

#[derive(Subcommand)]
pub enum Command {
    /// Own a new disposable headless Chromium/Edge profile until exit, timeout or Ctrl+C.
    Start {
        url: String,
        /// Browser executable. No shell is used.
        #[arg(long)] browser: PathBuf,
        /// Extra browser arguments, repeated as --browser-arg=VALUE. Profile overrides refused.
        #[arg(long)] browser_arg: Vec<String>,
        #[arg(long, default_value_t = 300)] seconds: u64,
    },
    /// Remove an inactive preview session by the ID printed by start. Never kills a process.
    Cleanup { session: String },
}

#[derive(Serialize, Deserialize)]
struct Record {
    owner_pid: u32,
    owner_started: Option<String>,
    browser_pid: u32,
    browser_started: Option<String>,
}

fn root() -> PathBuf { paths::data_dir().join("previews") }

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Start { url, browser, browser_arg, seconds } => {
            if !(1..=3600).contains(&seconds) { bail!("preview duration must be 1–3600 seconds"); }
            if !url.starts_with("http://") && !url.starts_with("https://") { bail!("preview URL must use http or https"); }
            if browser_arg.iter().any(|a| a.starts_with("--user-data-dir") || a.starts_with("--profile-directory")) {
                bail!("preview owns its disposable profile; profile overrides are not allowed");
            }
            let (stop, stopped) = std::sync::mpsc::sync_channel(1);
            ctrlc::set_handler(move || { let _ = stop.try_send(()); })?;
            let session = paths::unique_temp("preview", "").file_name().unwrap().to_string_lossy().into_owned();
            let folder = root().join(&session);
            std::fs::create_dir_all(root())?;
            std::fs::create_dir(&folder)?;
            let profile = folder.join("profile");
            std::fs::create_dir(&profile)?;
            let _profile_cleanup = ProfileCleanup { path: profile.clone(), session: session.clone() };
            let (owner_pid, owner_started) = procs::me();
            std::fs::write(folder.join("session.json"), serde_json::to_vec(&Record {
                owner_pid, owner_started: owner_started.clone(), browser_pid: 0, browser_started: None,
            })?)?;
            let guard = procs::TreeGuard::with(procs::Contain { memory_cap: None, low_priority: true, end_with_guard: true })
                .context("cannot create preview process containment")?;
            let mut cmd = std::process::Command::new(browser);
            cmd.args(browser_arg).args(["--headless=new", "--no-first-run", "--no-default-browser-check"])
                .arg(format!("--user-data-dir={}", profile.display())).arg(url)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            #[cfg(unix)]
            { use std::os::unix::process::CommandExt; cmd.process_group(0); }
            guard.prepare(&mut cmd);
            let child = cmd.spawn().context("starting preview browser")?;
            let mut owned = Owned { child, guard: Some(guard) };
            if !owned.guard.as_ref().unwrap().adopt(&owned.child) { bail!("cannot contain preview browser; stopping it"); }
            let record = Record { owner_pid, owner_started, browser_pid: owned.child.id(), browser_started: procs::started(owned.child.id()) };
            std::fs::write(folder.join("session.json"), serde_json::to_vec(&record)?)?;
            crate::print_json(&serde_json::json!({ "session": session, "profile": profile, "pid": record.browser_pid, "timeout_seconds": seconds }))?;
            let until = Instant::now() + Duration::from_secs(seconds);
            let outcome = loop {
                if let Some(status) = owned.child.try_wait()? { break if status.success() { Ok(()) } else { Err(anyhow::anyhow!("preview browser exited with {status}")) }; }
                if Instant::now() >= until { break Err(anyhow::anyhow!("preview timed out after {seconds} seconds; owned processes stopped")); }
                if stopped.recv_timeout(Duration::from_millis(100)).is_ok() { break Ok(()); }
            };
            drop(owned);
            // Keep only the small identity record when cleanup succeeds; the private profile is disposable.
            if let Err(error) = std::fs::remove_dir_all(&profile) {
                bail!("preview ended but profile cleanup failed: {error}; retry nitely preview cleanup {session}");
            }
            outcome
        }
        Command::Cleanup { session } => cleanup(&session),
    }
}

struct Owned { child: Child, guard: Option<procs::TreeGuard> }
struct ProfileCleanup { path: PathBuf, session: String }
impl Drop for ProfileCleanup {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("preview profile cleanup failed: {error}; retry nitely preview cleanup {}", self.session);
            }
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            unsafe extern "C" { fn kill(pid: i32, signal: i32) -> i32; }
            // The group was created only for this child; descendants cannot join a user browser group.
            if let Ok(pid) = i32::try_from(self.child.id()) { unsafe { kill(-pid, 9); } }
        }
        if let Some(guard) = self.guard.take() { guard.end(Duration::from_secs(5)); }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cleanup(session: &str) -> Result<()> {
    if !session.starts_with("preview-") || session.contains(['/', '\\']) || session.contains("..") { bail!("invalid preview session ID"); }
    let folder = root().join(session);
    let record: Record = serde_json::from_slice(&std::fs::read(folder.join("session.json"))?)?;
    let running = |pid, started: Option<&str>| procs::alive(pid) && procs::still_runs(pid, started);
    if running(record.owner_pid, record.owner_started.as_deref()) || running(record.browser_pid, record.browser_started.as_deref()) {
        bail!("preview session is active; stop its launcher first");
    }
    let (holders, complete) = procs::holders_of(&[Path::new(&folder)]);
    if !complete || holders.iter().any(|h| !h.is_empty()) { bail!("preview profile may still be in use; nothing removed"); }
    std::fs::remove_dir_all(&folder)?;
    crate::print_json(&serde_json::json!({ "removed": session }))
}
