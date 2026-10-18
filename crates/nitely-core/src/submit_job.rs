//! Asynchronous submit (NIT-212): an agent hands its branch in and moves on. The gate, the review
//! and everything `submit` does after them run as a job in a process of its own, and the result
//! comes back to the agent through `submit_status`, `next_task`, or a notice on its next nitely
//! call. docs/batching.md weighs this against batching.
//!
//! A job is `submit` itself, run later and elsewhere: one row in `submit_job`, the process that runs
//! it recorded by pid (a job whose process is gone is abandoned, as a lane run is), and its result
//! the same `Submitted` a blocking submit returns. Build lanes (NIT-197) run each request in the
//! process that asks for it, so the job's process is the requester of its gate's lane run once the
//! gate moves into lanes (NIT-198). `Runner` is how a job is started: a detached process for now;
//! a resident runner can take over by implementing it.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::submit::SubmitOptions;

/// A handed-in branch and what became of it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Job {
    pub id: i64,
    pub issue: String,
    pub worktree: String,
    pub branch: String,
    /// The commit handed in; the job submits whatever the branch holds when it runs.
    pub head: String,
    pub requested_by: String,
    pub skip_review: bool,
    pub local_only: bool,
    /// `queued`, `running`, `done` (submit finished: see `result`), `failed` (submit stopped with
    /// an error, or its process ended before it finished), or `superseded` (a newer submit of the
    /// same issue replaced it before it ran).
    pub state: String,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub pid: Option<i64>,
    /// Run by a process of its own (`nitely submit-job <id>`), not in its caller.
    pub detached: bool,
    /// What `submit` returned, as it does when it blocks.
    pub result: Option<Value>,
    pub error: Option<String>,
    /// When the submitter was told the outcome.
    pub delivered_at: Option<i64>,
}

impl Job {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "superseded")
    }

    /// One line for the submitter: "NIT-5: review passed, ready to land. Call land …".
    pub fn outcome(&self) -> String {
        match (self.state.as_str(), &self.result, &self.error) {
            ("done", Some(r), _) => {
                let state = r["state"].as_str().unwrap_or("");
                let next = r["next"].as_str().unwrap_or("");
                format!("{}: submitted, now {state}. {next}", self.issue)
            }
            ("failed", _, Some(e)) => format!("{}: the submit stopped: {e}", self.issue),
            ("superseded", _, _) => format!("{}: replaced by a newer submit before it ran.", self.issue),
            (state, _, _) => format!("{}: the submit is {state}.", self.issue),
        }
    }

    /// The submitter has work to do: the issue came back to it (findings, a failed gate, a push
    /// that failed) or the job failed.
    pub fn needs_the_submitter(&self) -> bool {
        self.state == "failed" || self.result.as_ref().is_some_and(|r| r["state"] == "In Progress")
    }
}

impl Db {
    /// The submit that came back to `requested_by` with work for it, in `project`, for `next_task`
    /// to hand back before any new issue: its issue's latest submit, while nothing has answered it
    /// since. One made moot by a later submit (a foreground one makes no job, but records a review)
    /// or by the issue closing is marked told, so it does not come back later.
    pub fn returned_submit(&self, requested_by: &str, project: &str) -> Result<Option<Job>> {
        let of_project = |key: &str| key.split_once('-').is_some_and(|(p, _)| p.eq_ignore_ascii_case(project));
        for j in self.undelivered_submit_jobs(requested_by)? {
            if !j.needs_the_submitter() || !of_project(&j.issue) {
                continue;
            }
            let latest = self.latest_submit_job(&j.issue)?.is_some_and(|l| l.id == j.id);
            let open = self.issue(&j.issue)?.is_some_and(|i| i.is_open());
            // Its own review, if it got that far, was recorded before it ended.
            let ended = j.ended_at.unwrap_or(j.queued_at);
            let reviewed_since: bool = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM review WHERE issue_key = ?1 COLLATE NOCASE AND ts > ?2)",
                params![j.issue, ended],
                |r| r.get(0),
            )?;
            if latest && open && !reviewed_since {
                return Ok(Some(j));
            }
            self.mark_submit_jobs_delivered(&[j.id])?;
        }
        Ok(None)
    }
}

const COLUMNS: &str =
    "id, issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at, started_at, ended_at, pid, result, error, delivered_at, detached";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Job> {
    let result: Option<String> = r.get(13)?;
    Ok(Job {
        id: r.get(0)?,
        issue: r.get(1)?,
        worktree: r.get(2)?,
        branch: r.get(3)?,
        head: r.get(4)?,
        requested_by: r.get(5)?,
        skip_review: r.get(6)?,
        local_only: r.get(7)?,
        state: r.get(8)?,
        queued_at: r.get(9)?,
        started_at: r.get(10)?,
        ended_at: r.get(11)?,
        pid: r.get(12)?,
        result: result.and_then(|t| serde_json::from_str(&t).ok()),
        error: r.get(14)?,
        delivered_at: r.get(15)?,
        detached: r.get(16)?,
    })
}

impl Db {
    /// A job, with a running one whose process is gone marked failed (abandoned).
    pub fn submit_job(&self, id: i64) -> Result<Option<Job>> {
        let job = self.conn.query_row(&format!("SELECT {COLUMNS} FROM submit_job WHERE id = ?1"), [id], row).optional()?;
        match job {
            Some(j) => Ok(Some(self.settle_abandoned(j)?)),
            None => Ok(None),
        }
    }

    /// An issue's latest job.
    pub fn latest_submit_job(&self, issue: &str) -> Result<Option<Job>> {
        let id: Option<i64> =
            self.conn.query_row("SELECT MAX(id) FROM submit_job WHERE issue_key = ?1 COLLATE NOCASE", [issue], |r| r.get(0)).optional()?.flatten();
        match id {
            Some(id) => self.submit_job(id),
            None => Ok(None),
        }
    }

    /// Finished jobs this requester has not been told about, oldest first. Queued and running ones
    /// are read too, so one whose process is gone is failed and told here.
    pub fn undelivered_submit_jobs(&self, requested_by: &str) -> Result<Vec<Job>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM submit_job WHERE requested_by = ?1 AND delivered_at IS NULL AND state IN ('done', 'failed', 'running', 'queued') ORDER BY id"
        ))?;
        let jobs: Vec<Job> = stmt.query_map([requested_by], row)?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for j in jobs {
            let j = self.settle_abandoned(j)?;
            if j.finished() {
                out.push(j);
            }
        }
        Ok(out)
    }

    pub fn mark_submit_jobs_delivered(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.conn.execute("UPDATE submit_job SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL", params![id, now()])?;
        }
        Ok(())
    }

    fn settle_abandoned(&self, mut j: Job) -> Result<Job> {
        // A queued job carries the pid of the process started for it, which may die waiting for the
        // issue's lock; a running one, the pid of the process running it.
        let Some(pid) = j.pid.map(|p| p as u32).filter(|p| *p != std::process::id()) else { return Ok(j) };
        if !matches!(j.state.as_str(), "queued" | "running") {
            return Ok(j);
        }
        // Long after the job started, its process id may be one the OS has given to another program
        // since: past that, the process must still be this job's (when its command line can be read).
        let since = j.started_at.unwrap_or(j.queued_at);
        let reused = || now() - since > PID_CHECK_AFTER_SECS && crate::procs::command_line(pid).is_some_and(|c| !runs_job(&c, &j));
        if process_runs(pid) && !reused() {
            return Ok(j);
        }
        let why = format!("{ABANDONED} (the machine restarted, or it was stopped); submit again");
        let failed = self.conn.execute(
            "UPDATE submit_job SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1 AND state IN ('queued', 'running')",
            params![j.id, now(), why],
        )?;
        if failed == 0 {
            // It finished between the read and now: its own record says how.
            return self.conn.query_row(&format!("SELECT {COLUMNS} FROM submit_job WHERE id = ?1"), [j.id], row).map_err(Into::into);
        }
        j.state = "failed".into();
        j.error = Some(why);
        j.ended_at = Some(now());
        Ok(j)
    }
}

const PID_CHECK_AFTER_SECS: i64 = 30 * 60;

/// Whether a process's command line is the one running `job`: `nitely submit-job <id>` for a job
/// run by a process of its own (a queued job's pid is always that process's), or a nitely for one
/// run in its caller.
fn runs_job(command_line: &str, job: &Job) -> bool {
    let c = command_line.to_lowercase();
    if job.detached || job.state == "queued" {
        c.split_whitespace().collect::<Vec<_>>().windows(2).any(|w| w[0] == "submit-job" && w[1] == job.id.to_string())
    } else {
        c.contains("nitely")
    }
}

/// How a job whose process died says so, which a crew supervisor reads to run it again.
pub const ABANDONED: &str = "its process ended before it finished";

/// Whether a process runs: asked of the OS directly, since a job's status is read often and a
/// wrong "gone" fails a job that is about to finish.
pub(crate) fn process_runs(pid: u32) -> bool {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn GetExitCodeProcess(process: *mut std::ffi::c_void, code: *mut u32) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
            fn GetLastError() -> u32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        const ERROR_INVALID_PARAMETER: u32 = 87;
        // SAFETY: a query handle opened, read and closed here.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // No such process; any other refusal (access denied) means it exists.
                return GetLastError() != ERROR_INVALID_PARAMETER;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            !ok || code == STILL_ACTIVE
        }
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).output().is_ok_and(|o| o.status.success())
    }
}

/// Starts a queued job somewhere that outlives the caller.
pub trait Runner {
    /// The id of the process started for the job, when there is one: a job whose process is gone
    /// before it ran is then failed rather than left queued.
    fn start(&self, job: &Job) -> Result<Option<u32>>;
}

/// Runs `nitely submit-job <id>` as a process of its own: hidden, in its own process group, its
/// output in the data folder's `jobs/<id>.log`, so it carries on after the agent and its MCP server
/// exit.
pub struct DetachedProcess {
    pub nitely: PathBuf,
}

/// Where a job's output goes.
pub fn log_path(id: i64) -> PathBuf {
    crate::paths::data_dir().join("jobs").join(format!("{id}.log"))
}

impl Runner for DetachedProcess {
    fn start(&self, job: &Job) -> Result<Option<u32>> {
        spawn_nitely(&self.nitely, &["submit-job".into(), job.id.to_string()], Path::new(&job.worktree), &log_path(job.id)).map(Some)
    }
}

/// Starts `nitely <args>` as a process of its own in `cwd`: hidden, in its own process group, its
/// output in `log`, so it carries on after its caller exits. Its pid.
pub fn spawn_nitely(nitely: &Path, args: &[String], cwd: &Path, log_path: &Path) -> Result<u32> {
    {
        use std::process::Stdio;
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path).with_context(|| format!("opening {}", log_path.display()))?;
        let mut cmd = std::process::Command::new(nitely);
        cmd.args(args).current_dir(cwd);
        cmd.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        // Not part of the agent's session: its reviewer starts as a fresh one (review.rs).
        for (name, _) in std::env::vars() {
            if crate::review::is_session_marker(&name) {
                cmd.env_remove(name);
            }
        }
        let mut child = spawn_detached(&mut cmd).with_context(|| format!("starting {} {}", nitely.display(), args.join(" ")))?;
        let pid = child.id();
        // Reaped when it ends, so a long-lived caller (an MCP server) keeps no zombie on Unix and a
        // crashed job reads as gone.
        std::thread::spawn(move || child.wait());
        Ok(pid)
    }
}

/// Starts `cmd` as a process of its own, apart from the caller's: no console window, its own
/// process group, and none of the caller's standard handles, so it outlives the caller (an MCP
/// server whose client left) and nobody reading the caller's output waits on it.
pub fn spawn_detached(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    without_inherited_std(|| cmd.spawn())
}

/// Runs `spawn` with this process's own standard handles kept from the child. Windows hands a new
/// process every inheritable handle, and this process's stdout is inheritable when it was itself
/// started with pipes (an MCP server's, or `nitely submit` run by a script): the job would hold that
/// pipe open, and whoever reads it would wait for the job to end.
#[cfg(windows)]
fn without_inherited_std<T>(spawn: impl FnOnce() -> T) -> T {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn GetHandleInformation(handle: isize, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    // STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE.
    let handles: Vec<(isize, u32)> = [(-10i32) as u32, (-11i32) as u32, (-12i32) as u32]
        .into_iter()
        .filter_map(|which| {
            // SAFETY: plain handle queries on this process's own standard handles.
            let h = unsafe { GetStdHandle(which) };
            let mut flags = 0u32;
            (h != 0 && h != -1 && unsafe { GetHandleInformation(h, &mut flags) } != 0).then_some((h, flags))
        })
        .collect();
    for (h, _) in &handles {
        // SAFETY: only the inherit flag of a valid handle of this process changes.
        unsafe { SetHandleInformation(*h, HANDLE_FLAG_INHERIT, 0) };
    }
    let out = spawn();
    for (h, flags) in &handles {
        // SAFETY: as above, back to what it was.
        unsafe { SetHandleInformation(*h, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT) };
    }
    out
}

#[cfg(not(windows))]
fn without_inherited_std<T>(spawn: impl FnOnce() -> T) -> T {
    spawn()
}

/// Records a submit to run later: the cheap checks `submit` would fail on at once are made here, so
/// the agent hears about them now. A queued job of the same issue is superseded.
pub fn enqueue(db: &Db, opts: &SubmitOptions, actor: &Actor) -> Result<Job> {
    let (worktree, branch_key) = crate::playbook::current_worktree(opts.dir)?;
    let key = opts.issue_key.map(str::to_string).or(branch_key).context("the branch names no issue; pass the issue key")?;
    let issue = db.issue(&key)?.with_context(|| format!("no issue {key}"))?;
    let git = |args: &[&str]| crate::playbook::git(&worktree, args);
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
    let head = git(&["rev-parse", "HEAD"])?.trim().to_string();
    let dirty = git(&["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"])?;
    if !dirty.trim().is_empty() {
        bail!("{} has uncommitted changes; commit them first:\n{}", worktree.display(), dirty.trim_end());
    }
    let wt = worktree.to_string_lossy().into_owned();
    db.conn.execute(
        "UPDATE submit_job SET state = 'superseded', ended_at = ?2 WHERE issue_key = ?1 COLLATE NOCASE AND state = 'queued'",
        params![issue.key, now()],
    )?;
    // One that already waits for a review batch is replaced too: its job then ends as superseded.
    crate::review_batch::supersede_waiting(db, &issue.key)?;
    db.conn.execute(
        "INSERT INTO submit_job (issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8)",
        params![issue.key, wt, branch, head, actor.id(), opts.skip_review, opts.local_only, now()],
    )?;
    let id = db.conn.last_insert_rowid();
    let short = &head[..head.len().min(7)];
    db.log_event(actor, Some(&issue.key), Some(&wt), "submit_queued", &format!("Submitted {short} for the gate and review (job {id})"))?;
    db.submit_job(id)?.context("the job's record went missing")
}

/// The next step for an agent that just handed a branch in.
pub const QUEUED_NEXT: &str = "Handed in: the gate and the review run in the background. Pick up other work (next_task) or stop. \
The result comes back through next_task, submit_status, or a notice on your next nitely call.";

/// Hands a branch in and starts its job. When the job cannot be started (no way to start a
/// process here), it runs now, in this process, and says so.
pub fn submit_async(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, actor: &Actor, runner: &dyn Runner) -> Result<Job> {
    let job = enqueue(db, opts, actor)?;
    match runner.start(&job) {
        Err(e) => {
            db.log_event(actor, Some(&job.issue), Some(&job.worktree), "status", &format!("Could not start submit job {} in the background ({e:#}); running it here", job.id))?;
            run(db, config, job.id)
        }
        Ok(Some(pid)) => {
            db.conn.execute("UPDATE submit_job SET pid = ?2 WHERE id = ?1 AND state = 'queued' AND pid IS NULL", params![job.id, pid])?;
            db.submit_job(job.id)?.context("the job's record went missing")
        }
        Ok(None) => Ok(job),
    }
}

/// Runs a job: the job's own process calls this. One job of an issue runs at a time (a file lock
/// per issue); a job superseded while it waited does nothing.
pub fn run(db: &mut Db, config: &crate::Config, id: i64) -> Result<Job> {
    run_job(db, config, id, false).map(|(job, _)| job)
}

/// `run`, then, once the job has handed its result back and released the issue, its change's
/// shadow batch (`[review] batch = "shadow"`, NIT-217). Only the job's own process does this: it
/// waits for the batch's window, and nobody waits on that process.
pub fn run_in_background(db: &mut Db, config: &crate::Config, id: i64) -> Result<Job> {
    let (job, shadow) = run_job(db, config, id, true)?;
    if let Some(shadow) = shadow {
        if let Err(e) = crate::review_batch::shadow(db, &shadow) {
            eprintln!("the shadow batch of {} did not finish: {e:#}", job.issue);
        }
    }
    Ok(job)
}

/// `detached`: this is the job's own process, which nobody waits on; otherwise the job runs in its
/// caller (no process could be started), which waits for it, so it never waits for a review batch.
fn run_job(db: &mut Db, config: &crate::Config, id: i64, detached: bool) -> Result<(Job, Option<crate::review_batch::Shadow>)> {
    let job = db.submit_job(id)?.with_context(|| format!("no submit job {id}"))?;
    if job.state != "queued" {
        return Ok((job, None));
    }
    let locks = crate::paths::data_dir().join("jobs");
    std::fs::create_dir_all(&locks)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(locks.join(format!("{}.lock", job.issue.to_uppercase())))?;
    lock.lock().context("waiting for the issue's previous submit")?;
    let started = db.conn.execute(
        "UPDATE submit_job SET state = 'running', started_at = ?2, pid = ?3, detached = ?4 WHERE id = ?1 AND state = 'queued'",
        params![id, now(), std::process::id(), detached],
    )?;
    if started == 0 {
        // Superseded while it waited for the lock.
        return Ok((db.submit_job(id)?.context("the job's record went missing")?, None));
    }
    let actor = if job.requested_by == "human" { Actor::Human } else { Actor::Agent(job.requested_by.clone()) };
    // Only a submit in the background may wait for a review batch: nobody waits on it (NIT-216).
    let opts = SubmitOptions { dir: &job.worktree, issue_key: Some(&job.issue), skip_review: job.skip_review, local_only: job.local_only, batch: detached };
    let outcome = crate::submit::submit(db, config, &opts, &actor);
    match &outcome {
        Ok(s) => {
            db.conn.execute(
                "UPDATE submit_job SET state = 'done', ended_at = ?2, result = ?3 WHERE id = ?1",
                params![id, now(), serde_json::to_string(s)?],
            )?;
        }
        Err(e) if e.downcast_ref::<crate::review_batch::Superseded>().is_some() => {
            db.conn.execute("UPDATE submit_job SET state = 'superseded', ended_at = ?2, error = ?3 WHERE id = ?1", params![id, now(), e.to_string()])?;
        }
        Err(e) => {
            let why = format!("{e:#}");
            db.conn.execute("UPDATE submit_job SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1", params![id, now(), why])?;
            db.add_note(&job.issue, &format!("The submit (job {id}) stopped: {why}"), "submit", &actor)?;
        }
    }
    drop(lock);
    let shadow = outcome.ok().and_then(|s| s.shadow);
    Ok((db.submit_job(id)?.context("the job's record went missing")?, shadow))
}

/// Waits up to `limit` for a job to finish, and returns it as it stands then.
pub fn wait(db: &Db, id: i64, limit: Duration) -> Result<Job> {
    let deadline = Instant::now() + limit;
    loop {
        let job = db.submit_job(id)?.with_context(|| format!("no submit job {id}"))?;
        if job.finished() || Instant::now() >= deadline {
            return Ok(job);
        }
        std::thread::sleep(Duration::from_millis(500).min(deadline.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;
    use crate::playbook;
    use std::path::Path;
    use std::process::Command;

    /// A repo with a playbook whose gate passes and whose only reviewer passes everything, an issue
    /// with a worktree and a commit on its branch.
    fn fixture() -> (PathBuf, PathBuf, crate::Config, String, PathBuf) {
        let base = crate::paths::unique_temp("nitely-submit-job", "");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            let o = playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("review.sh"), "cat > /dev/null\necho 'Fine.'\necho 'VERDICT: clean'\n").unwrap();
        std::fs::write(
            repo.join(playbook::PLAYBOOK_PATH),
            "[worktree]\ndir = \"../wt/{key}\"\n[gate]\ncommands = [\"exit 0\"]\n[review]\nreviewer = { \"*\" = \"claude\" }\ncommands = { claude = [\"{sh}\", \"review.sh\"] }\n"
                .replace("{sh}", &playbook::posix_sh()),
        )
        .unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "init"]);
        let db_path = base.join("t.db");
        let mut db = Db::open(&db_path).unwrap();
        db.create_project("APP", "App").unwrap();
        let actor = Actor::Agent("codex:session-1".into());
        let issue = db.create_issue(&NewIssue { project: "APP".into(), title: "Footer link".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() }, &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "codex", &actor).unwrap();
        let wt = PathBuf::from(&started.worktree);
        std::fs::write(wt.join("footer.txt"), "x").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "footer"]);
        let config = crate::Config::default();
        (base, db_path, config, issue.key, wt)
    }

    /// Records a start without starting anything.
    struct Recorded(std::sync::Mutex<Vec<i64>>);
    impl Runner for Recorded {
        fn start(&self, job: &Job) -> Result<Option<u32>> {
            self.0.lock().unwrap().push(job.id);
            Ok(None)
        }
    }

    #[test]
    fn a_submit_hands_in_at_once_and_the_job_brings_the_result_back() {
        let (base, db_path, config, key, wt) = fixture();
        let mut db = Db::open(&db_path).unwrap();
        let actor = Actor::Agent("codex:session-1".into());
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false };

        // Handed in: queued, nothing ran, the issue still In Progress.
        let runner = Recorded(Default::default());
        let job = submit_async(&mut db, &config, &opts, &actor, &runner).unwrap();
        assert_eq!((job.state.as_str(), job.issue.as_str()), ("queued", key.as_str()));
        assert_eq!(*runner.0.lock().unwrap(), [job.id]);
        assert_eq!(db.issue(&key).unwrap().unwrap().state, "In Progress");
        assert!(db.undelivered_submit_jobs(actor.id()).unwrap().is_empty(), "nothing to tell yet");

        // A second submit before it ran replaces it.
        let again = submit_async(&mut db, &config, &opts, &actor, &runner).unwrap();
        assert_eq!(db.submit_job(job.id).unwrap().unwrap().state, "superseded");
        assert_eq!(run(&mut db, &config, job.id).unwrap().state, "superseded", "a superseded job does nothing");

        // The job runs submit as a blocking submit would, and its result waits for the submitter.
        let done = run(&mut db, &config, again.id).unwrap();
        assert_eq!(done.state, "done", "{:?}", done.error);
        let result = done.result.clone().unwrap();
        assert_eq!(result["state"], "In Review", "{result}");
        assert_eq!(db.issue(&key).unwrap().unwrap().state, "In Review");
        assert!(done.outcome().starts_with(&format!("{key}: submitted, now In Review.")), "{}", done.outcome());
        assert!(!done.needs_the_submitter());
        let told = db.undelivered_submit_jobs(actor.id()).unwrap();
        assert_eq!(told.iter().map(|j| j.id).collect::<Vec<_>>(), [again.id]);
        assert!(db.undelivered_submit_jobs("claude:someone-else").unwrap().is_empty(), "only to the agent that submitted");
        db.mark_submit_jobs_delivered(&[again.id]).unwrap();
        assert!(db.undelivered_submit_jobs(actor.id()).unwrap().is_empty());
        assert_eq!(wait(&db, again.id, Duration::ZERO).unwrap().state, "done");

        // Uncommitted work is refused at once, before anything is queued.
        std::fs::write(wt.join("footer.txt"), "y").unwrap();
        let err = submit_async(&mut db, &config, &opts, &actor, &runner).unwrap_err().to_string();
        assert!(err.contains("uncommitted changes"), "{err}");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_job_whose_process_is_gone_is_failed_and_one_that_cannot_start_runs_here() {
        let (base, db_path, config, key, wt) = fixture();
        let mut db = Db::open(&db_path).unwrap();
        let actor = Actor::Agent("codex:session-1".into());
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false };
        let job = enqueue(&db, &opts, &actor).unwrap();
        // Running under a process that no longer exists: abandoned, and the submitter is told.
        db.conn.execute("UPDATE submit_job SET state = 'running', pid = 999999, started_at = ?2 WHERE id = ?1", rusqlite::params![job.id, now()]).unwrap();
        assert!(process_runs(std::process::id()) && !process_runs(999999));
        let gone = db.submit_job(job.id).unwrap().unwrap();
        assert_eq!(gone.state, "failed");
        assert!(gone.error.as_deref().unwrap().contains("process ended"), "{gone:?}");
        assert!(gone.needs_the_submitter());
        assert_eq!(db.undelivered_submit_jobs(actor.id()).unwrap().len(), 1);

        // Queued, its process gone before it ran (killed while it waited for the issue's lock):
        // failed too, not left queued forever.
        struct Started;
        impl Runner for Started {
            fn start(&self, _: &Job) -> Result<Option<u32>> {
                Ok(Some(999999))
            }
        }
        let waiting = submit_async(&mut db, &config, &opts, &actor, &Started).unwrap();
        assert_eq!((waiting.state.as_str(), waiting.pid), ("failed", Some(999999)), "{waiting:?}");
        // And the submitter hears of one without asking for it.
        let unasked = enqueue(&db, &opts, &actor).unwrap();
        db.conn.execute("UPDATE submit_job SET pid = 999999 WHERE id = ?1", [unasked.id]).unwrap();
        let told = db.undelivered_submit_jobs(actor.id()).unwrap();
        assert!(told.iter().any(|j| j.id == unasked.id && j.state == "failed"), "{told:?}");

        // A runner that cannot start a process: the job runs in this one, and finishes.
        struct Broken;
        impl Runner for Broken {
            fn start(&self, _: &Job) -> Result<Option<u32>> {
                bail!("no way to start a process here")
            }
        }
        let here = submit_async(&mut db, &config, &opts, &actor, &Broken).unwrap();
        assert_eq!(here.state, "done", "{:?}", here.error);
        assert_eq!(db.issue(&key).unwrap().unwrap().state, "In Review");
        let said: Vec<String> = db.timeline(&key).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(said.iter().any(|s| s.contains("running it here")), "{said:?}");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_failed_submit_comes_back_only_while_its_issue_waits_for_it() {
        let (base, db_path, _config, key, wt) = fixture();
        let mut db = Db::open(&db_path).unwrap();
        let actor = Actor::Agent("codex:session-1".into());
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false };
        let job = enqueue(&db, &opts, &actor).unwrap();
        db.conn.execute("UPDATE submit_job SET state = 'failed', error = 'the gate failed', ended_at = ?2 WHERE id = ?1", params![job.id, now()]).unwrap();
        let project = key.split('-').next().unwrap().to_string();
        assert_eq!(db.returned_submit(actor.id(), &project).unwrap().map(|j| j.id), Some(job.id));
        assert_eq!(db.returned_submit(actor.id(), "OTHER").unwrap(), None, "only the project asked for");
        // Still In Review from an earlier pass, its new submit failed: that failure comes back.
        db.move_issue(&key, "In Review", None, None, &actor).unwrap();
        assert_eq!(db.returned_submit(actor.id(), &project).unwrap().map(|j| j.id), Some(job.id));
        // A foreground submit reviewed it since (it makes no job): the failure is moot, and told.
        db.conn.execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, ts) VALUES (?1, 'p', 'c', 'codex', 'pass', 'agent', ?2)",
            params![key, now() + 1],
        )
        .unwrap();
        assert_eq!(db.returned_submit(actor.id(), &project).unwrap(), None);
        assert!(db.undelivered_submit_jobs(actor.id()).unwrap().is_empty());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_reused_process_id_is_not_the_job() {
        let job = |state: &str, detached: bool| Job {
            id: 42,
            issue: "APP-1".into(),
            worktree: String::new(),
            branch: String::new(),
            head: String::new(),
            requested_by: String::new(),
            skip_review: false,
            local_only: false,
            state: state.into(),
            queued_at: 0,
            started_at: None,
            ended_at: None,
            pid: Some(7),
            detached,
            result: None,
            error: None,
            delivered_at: None,
        };
        let own = r#"C:\Users\me\.cargo\bin\nitely.exe submit-job 42"#;
        assert!(runs_job(own, &job("running", true)) && runs_job(own, &job("queued", false)));
        // Another job's process, or a long-lived nitely that took the number: not this job.
        assert!(!runs_job(r#"C:\Users\me\.cargo\bin\nitely.exe submit-job 420"#, &job("running", true)));
        assert!(!runs_job(r#"C:\Users\me\.cargo\bin\nitely.exe mcp"#, &job("running", true)));
        // Run in its caller (no process could be started): that caller is a nitely.
        assert!(runs_job(r#"C:\Users\me\.cargo\bin\nitely.exe mcp"#, &job("running", false)));
        assert!(!runs_job(r#"C:\Windows\System32\notepad.exe"#, &job("running", false)));
    }
}
