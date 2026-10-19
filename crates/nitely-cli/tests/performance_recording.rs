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
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() { assert!(status.success()); break; }
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
