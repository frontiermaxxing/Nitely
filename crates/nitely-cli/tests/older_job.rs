//! A submit job whose process finds the database upgraded past its build: it hands the job to the
//! newer build once that is installed at its path, and when none comes, the job says why it
//! stopped instead of "its process ended" with no reason (NIT-314).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// A migration no build has: the database reads as upgraded by a newer Nitely.
const FUTURE: &str = "29991231-from-a-newer-build";

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

/// Puts a "newer" build at `exe` the way `nitely install` does: the running one moved aside, the
/// new one in its place, with a stamp of its own.
fn install_over(exe: &Path) {
    std::fs::rename(exe, exe.with_file_name(format!("nitely-old{}", std::env::consts::EXE_SUFFIX))).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_nitely"), exe).unwrap();
    std::fs::File::options().write(true).open(exe).unwrap().set_modified(SystemTime::now() + Duration::from_secs(60)).unwrap();
}

/// Runs `nitely submit-job <id>` from `exe` the way submit starts it, and waits for it.
fn run_job(base: &Path, exe: &Path, id: i64, install_wait: u64, meanwhile: impl FnOnce()) -> std::process::ExitStatus {
    let mut child = Command::new(exe)
        .args(["submit-job", &id.to_string()])
        .current_dir(base)
        .env("NITELY_HOME", base)
        .env("NITELY_MCP_INSTALL_WAIT", install_wait.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
fn a_job_that_meets_a_newer_database_runs_in_the_build_installed_meanwhile() {
    let (base, exe, id) = setup("nitely-older-job-install");
    run_job(&base, &exe, id, 30, || {
        // The install lands a moment after the job started: the newer build, which knows the
        // database (here: the migration it needs is gone), at the job's path.
        std::thread::sleep(Duration::from_millis(1500));
        set_future(&base, false);
        install_over(&exe);
    });
    // The job ran in the new build: it got as far as the submit itself, which has no worktree here.
    let (state, error) = job(&base, id);
    assert_eq!(state, "failed", "{error}");
    assert!(!error.contains("older than the database") && !error.contains("process ended"), "the submit ran: {error}");
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn with_nothing_newer_installed_the_job_says_why_it_stopped() {
    let (base, exe, id) = setup("nitely-older-job-none");
    let status = run_job(&base, &exe, id, 2, || {});
    assert!(!status.success());
    let (state, error) = job(&base, id);
    assert_eq!(state, "failed");
    assert!(error.contains("older than the database") && error.contains("no newer build was installed"), "{error}");
    // A crew supervisor hands it in again, as it does a job whose process died.
    assert!(error.contains(nitely_core::submit_job::ABANDONED), "{error}");
    assert!(!error.contains("machine restarted"), "{error}");
    std::fs::remove_dir_all(base).ok();
}
