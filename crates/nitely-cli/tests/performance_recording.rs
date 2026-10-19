//! Exercise recorder shutdown through the real CLI, on an isolated data directory.
use nitely_core::performance::Store;
use std::{path::PathBuf, process::{Child, Command, Stdio}, time::{Duration, Instant}};

struct Recorder { child: Child, home: PathBuf }
impl Recorder {
    fn start(args: &[&str]) -> Self {
        let home = nitely_core::paths::unique_temp("performance-shutdown", "");
        std::fs::create_dir_all(&home).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_nitely")).args(["performance", "record"]).args(args)
            .env("NITELY_HOME", &home).current_dir(&home).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        Self { child, home }
    }
    fn wait_for_sample(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Store::read(&self.home.join("performance")).ok().flatten()
                .is_some_and(|store| store.samples(0, i64::MAX, 1).is_ok_and(|samples| !samples.is_empty())) { return; }
            assert!(self.child.try_wait().unwrap().is_none(), "recorder exited before saving a sample");
            assert!(Instant::now() < deadline, "recorder did not save a sample");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn wait_for_clean_exit(&mut self) {
        self.wait_for_exit_code(0);
    }
    fn wait_for_exit_code(&mut self, code: i32) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() { assert_eq!(status.code(), Some(code)); break; }
            assert!(Instant::now() < deadline, "recorder did not shut down");
            std::thread::sleep(Duration::from_millis(25));
        }
        let store = Store::read(&self.home.join("performance")).unwrap().unwrap();
        let sessions = store.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].stopped_ms.is_some(), "normal shutdown must be durable before exit");
        assert!(!store.samples(0, i64::MAX, 1).unwrap().is_empty());
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() { let _ = self.child.kill(); }
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn performance_record_duration_stops_cleanly() {
    let mut recorder = Recorder::start(&["--seconds", "5"]);
    recorder.wait_for_sample();
    recorder.wait_for_clean_exit();
}

#[test]
fn performance_record_abrupt_exit_remains_unexplained() {
    let mut recorder = Recorder::start(&[]);
    recorder.wait_for_sample();
    recorder.child.kill().unwrap();
    recorder.child.wait().unwrap();
    let store = Store::read(&recorder.home.join("performance")).unwrap().unwrap();
    assert!(store.sessions().unwrap()[0].stopped_ms.is_none());
    assert!(!store.samples(0, i64::MAX, 1).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn performance_record_ctrl_c_stops_cleanly() {
    let mut recorder = Recorder::start(&[]);
    recorder.wait_for_sample();
    assert!(Command::new("kill").args(["-INT", &recorder.child.id().to_string()]).status().unwrap().success());
    recorder.wait_for_clean_exit();
}

fn command_exit(args: &[&str], child_code: i32, expected_code: i32) {
    let home = nitely_core::paths::unique_temp("performance-command-exit", "");
    let repo = home.join("repo");
    std::fs::create_dir_all(repo.join(".nitely")).unwrap();
    std::fs::write(repo.join(".nitely/playbook.toml"), "[lanes]\ncount = 1\n[gate]\ncommands = [\"node release.js\"]\n").unwrap();
    // Hold the command until a persisted sample proves this CLI owns a recording session. This
    // avoids assuming a fast command happens to give the background collector time to start.
    std::fs::write(repo.join("release.js"), format!(
        "const fs = require('fs'); const until = Date.now() + 30000;\n(function wait() {{ if (fs.existsSync(process.env.NITELY_TEST_RECORDING_RELEASE)) process.exit({child_code}); else if (Date.now() > until) process.exit(99); else setTimeout(wait, 25); }})();\n"
    )).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["add", "."], &["commit", "-qm", "fixture"]] {
        let output = nitely_core::playbook::run_git(Command::new("git").current_dir(&repo)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.test"]).args(args)).unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let child = Command::new(env!("CARGO_BIN_EXE_nitely")).args(args).current_dir(&repo)
        .env("NITELY_HOME", &home).env("NITELY_MEMORY_FLOOR_GB", "0")
        .env("NITELY_TEST_RECORDING_RELEASE", home.join("release"))
        .stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
    let mut recorder = Recorder { child, home };
    recorder.wait_for_sample();
    std::fs::write(recorder.home.join("release"), "").unwrap();
    recorder.wait_for_exit_code(expected_code);
}

#[test]
fn successful_lane_run_records_clean_exit() {
    command_exit(&["run", "--shell", "node release.js"], 0, 0);
}

#[test]
fn failed_lane_run_records_clean_exit_and_preserves_status() {
    command_exit(&["run", "--shell", "node release.js"], 7, 7);
}

#[test]
fn failed_gate_records_clean_exit_and_preserves_status() {
    command_exit(&["gate"], 7, 1);
}
