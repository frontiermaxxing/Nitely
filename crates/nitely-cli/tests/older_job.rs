//! A submit job whose process finds the database upgraded past its build, and no newer build comes:
//! the job says why it stopped instead of "its process ended" with no reason (NIT-314). The hand-over
//! to a build installed meanwhile is part of mcp_stale's install scenario (NIT-528).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A migration no build has: the database reads as upgraded by a newer Nitely.
const FUTURE: &str = "29991231-from-a-newer-build";

/// Starts a copy of the built binary. On Linux a test's child forked while the copy was still open
/// for writing holds that descriptor until it execs, and exec fails with "Text file busy" meanwhile
/// (NIT-541): that error is tried again, briefly; any other is not.
fn spawn(command: &mut Command) -> Child {
    for _ in 0..100 {
        match command.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(Duration::from_millis(20)),
            started => return started.unwrap(),
        }
    }
    command.spawn().unwrap()
}

/// A Nitely home with one queued submit job, its database upgraded by a "newer" build, and this
/// build copied to `bin/` as if installed there.
fn setup(name: &str) -> (PathBuf, PathBuf, i64) {
    let base = nitely_core::paths::unique_temp(name, "");
    std::fs::create_dir_all(base.join("bin")).unwrap();
    let db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    drop(db);
    let conn = rusqlite::Connection::open(base.join("nitely.db")).unwrap();
    conn
        .execute(
            "INSERT INTO submit_job (issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at)
             VALUES ('APP-1', ?1, 'claude/app-1-x', 'abc1234', 'claude:test', 0, 1, 'queued', 1)",
            [base.join("no-such-worktree").to_string_lossy()],
        )
        .unwrap();
    let id = conn.last_insert_rowid();
    drop(conn);
    set_future(&base, true);
    let exe = base.join("bin").join(format!("nitely{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_nitely"), &exe).unwrap();
    (base, exe, id)
}

fn set_future(base: &Path, on: bool) {
    let conn = rusqlite::Connection::open(base.join("nitely.db")).unwrap();
    if on {
        conn.execute("INSERT INTO schema_migration (name, applied_at, applied_by) VALUES (?1, 0, 'a newer build')", [FUTURE]).unwrap();
    } else {
        conn.execute("DELETE FROM schema_migration WHERE name = ?1", [FUTURE]).unwrap();
    }
}

/// Runs `nitely submit-job <id>` from `exe` the way submit starts it, and waits for it.
fn run_job(base: &Path, exe: &Path, id: i64, install_wait: u64, meanwhile: impl FnOnce()) -> std::process::ExitStatus {
    let mut child = spawn(
        Command::new(exe)
            .args(["submit-job", &id.to_string()])
            .current_dir(base)
            .env("NITELY_HOME", base)
            .env("NITELY_MCP_INSTALL_WAIT", install_wait.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    meanwhile();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the job's process did not end");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn job(base: &Path, id: i64) -> (String, String) {
    let conn = rusqlite::Connection::open(base.join("nitely.db")).unwrap();
    conn.query_row("SELECT state, COALESCE(error, '') FROM submit_job WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
}

#[test]
fn with_nothing_newer_installed_the_job_says_why_it_stopped() {
    let (base, exe, id) = setup("nitely-older-job-none");
    let status = run_job(&base, &exe, id, 1, || {});
    assert!(!status.success());
    let (state, error) = job(&base, id);
    assert_eq!(state, "failed");
    assert!(error.contains("older than the database") && error.contains("no newer build was installed"), "{error}");
    // A crew supervisor hands it in again, as it does a job whose process died.
    assert!(error.contains(nitely_core::submit_job::ABANDONED), "{error}");
    assert!(!error.contains("machine restarted"), "{error}");
    std::fs::remove_dir_all(base).ok();
}
