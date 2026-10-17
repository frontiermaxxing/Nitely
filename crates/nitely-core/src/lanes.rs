//! Build lanes (docs/build-lanes.md): warm, full checkouts of a repo that keep their build output,
//! where builds and tests run as requests instead of in every agent's worktree. A request is a
//! snapshot commit and a command. A lane serves it in the same four steps whatever the last request
//! left: a forced checkout of the snapshot, a clean of everything untracked except the playbook's
//! build directories, setup again when a dependency file changed, then the command. A lane holds
//! nobody's work, so the worst a crash can do is leave it to be cleaned by the next request.
//!
//! Nothing here runs for a repo whose playbook has no `[lanes]`.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::playbook::{self, git, Playbook};

/// A playbook's `[lanes]`. Without the table a repo has no lanes and everything works as before.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneRules {
    /// Lanes for this repo on this machine.
    pub count: u32,
    /// Build directories (repo-relative) a lane keeps between requests; `target` and `node_modules`
    /// unless the playbook says (the same list `nitely lanes slim` reclaims).
    pub keep: Vec<String>,
    /// A command running longer is stopped, with everything it started.
    pub timeout_minutes: u64,
}

impl Default for LaneRules {
    fn default() -> Self {
        LaneRules { count: 1, keep: crate::slim::DEFAULT_KEEP.iter().map(|s| s.to_string()).collect(), timeout_minutes: 30 }
    }
}

/// The folder a repo's lanes live in, beside its reference tree: `.nitely-lanes/<repo>` next to its
/// worktrees (light.rs), which no listing of agents' worktrees shows.
pub fn dir(repo_root: &Path, pb: &Playbook) -> PathBuf {
    crate::light::lanes_dir(pb, repo_root)
}

fn lane_path(repo_root: &Path, pb: &Playbook, n: u32) -> PathBuf {
    dir(repo_root, pb).join(format!("lane-{n}"))
}

// ----- snapshots -----

/// What a request builds: a commit in the repo's object store.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    /// The commit the worktree's work sits on (its HEAD).
    pub base: String,
    /// The worktree had uncommitted work, so `commit` was made for this request.
    pub made: bool,
}

/// `worktree` as a commit: its HEAD when nothing is uncommitted, else a commit built in a scratch
/// index from HEAD plus every path `git status` lists. The agent's files, index and branch are
/// never touched. Paths outside a sparse checkout are not listed, so they stay as in HEAD.
pub fn snapshot(worktree: &Path) -> Result<Snapshot> {
    let top = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"])?.trim());
    let head = git(&top, &["rev-parse", "HEAD"])?.trim().to_string();
    let status = git(&top, &["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=all"])?;
    let paths = changed_paths(&status);
    if paths.is_empty() {
        let tree = git(&top, &["rev-parse", "HEAD^{tree}"])?.trim().to_string();
        return Ok(Snapshot { commit: head.clone(), tree, base: head, made: false });
    }
    let scratch = crate::paths::unique_temp("nitely-snapshot", "");
    std::fs::create_dir_all(&scratch)?;
    let index = scratch.join("index");
    let result = (|| -> Result<(String, String)> {
        let with_index = |args: &[&str], input: Option<&[u8]>| -> Result<String> {
            use std::io::Write;
            let mut cmd = playbook::shell_free_command("git");
            cmd.arg("-C").arg(&top).env("GIT_INDEX_FILE", &index).args(args);
            cmd.env("GIT_AUTHOR_NAME", "Nitely").env("GIT_AUTHOR_EMAIL", "nitely@localhost");
            cmd.env("GIT_COMMITTER_NAME", "Nitely").env("GIT_COMMITTER_EMAIL", "nitely@localhost");
            cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn()?;
            if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
                stdin.write_all(bytes)?;
            }
            let out = child.wait_with_output()?;
            if !out.status.success() {
                bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        with_index(&["read-tree", "HEAD"], None)?;
        let mut list = Vec::new();
        for p in &paths {
            list.extend_from_slice(p.as_bytes());
            list.push(0);
        }
        // Each path as the worktree has it now: added or updated when present, removed when gone.
        // --replace: a folder that became a file (or the other way) replaces what the index had.
        with_index(&["update-index", "--add", "--remove", "--replace", "-z", "--stdin"], Some(&list))?;
        let tree = with_index(&["write-tree"], None)?;
        let commit = with_index(&["commit-tree", &tree, "-p", &head, "-m", "nitely run: uncommitted work"], None)?;
        Ok((commit, tree))
    })();
    std::fs::remove_dir_all(&scratch).ok();
    let (commit, tree) = result?;
    Ok(Snapshot { commit, tree, base: head, made: true })
}

/// The paths `git status --porcelain=v2 -z` names: changed, renamed (both sides), unmerged and
/// untracked.
fn changed_paths(status: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut entries = status.split('\0');
    while let Some(e) = entries.next() {
        let mut fields = e.splitn(2, ' ');
        let kind = fields.next().unwrap_or("");
        let path = match kind {
            "1" => e.splitn(9, ' ').nth(8),
            "2" => {
                let p = e.splitn(10, ' ').nth(9);
                // The original path follows as its own entry.
                if let Some(orig) = entries.next() {
                    out.push(orig.to_string());
                }
                p
            }
            "u" => e.splitn(11, ' ').nth(10),
            "?" => fields.next(),
            _ => None,
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            out.push(p.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

// ----- the request -----

/// What a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An agent's or a person's `run`.
    Run,
    /// submit's gate (NIT-198).
    Gate,
    /// land's gate after its rebase (NIT-198).
    Land,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Run => "run",
            Kind::Gate => "gate",
            Kind::Land => "land",
        }
    }
}

pub struct Request<'a> {
    pub project: &'a str,
    pub issue: Option<&'a str>,
    pub kind: Kind,
    pub snapshot: &'a Snapshot,
    pub command: &'a str,
}

/// A request and what came of it: the `lane_run` row. NIT-205 builds the app's view on this.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: i64,
    pub project: String,
    pub repo: String,
    pub issue: Option<String>,
    pub by: String,
    pub kind: String,
    pub snapshot: String,
    pub tree: String,
    pub base: String,
    pub command: String,
    /// queued | running | done | lane_error | abandoned
    pub state: String,
    /// The lane's number (1..count), once it has one.
    pub lane: Option<u32>,
    pub executor: String,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    /// The command's exit code; None until it ends, or when it was stopped or never ran.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Setup ran again first, because a dependency file differed from the lane's last setup.
    pub setup_ran: bool,
    /// Files the checkout wrote to switch the lane to this snapshot.
    pub files_switched: Option<i64>,
    pub log: Option<String>,
    /// The end of the output.
    pub output: String,
    /// Why the lane could not run it (state lane_error), in words.
    pub error: Option<String>,
}

impl Run {
    /// The command ran and exited 0.
    pub fn passed(&self) -> bool {
        self.state == "done" && self.exit_code == Some(0)
    }
}

const COLUMNS: &str = "id, project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, lane, executor, \
queued_at, started_at, ended_at, exit_code, timed_out, setup_ran, files_switched, log_path, output, error, pid";

fn row(r: &rusqlite::Row) -> rusqlite::Result<(Run, Option<i64>)> {
    Ok((
        Run {
            id: r.get(0)?,
            project: r.get(1)?,
            repo: r.get(2)?,
            issue: r.get(3)?,
            by: r.get(4)?,
            kind: r.get(5)?,
            snapshot: r.get(6)?,
            tree: r.get(7)?,
            base: r.get(8)?,
            command: r.get(9)?,
            state: r.get(10)?,
            lane: r.get::<_, Option<i64>>(11)?.map(|n| n as u32),
            executor: r.get(12)?,
            queued_at: r.get(13)?,
            started_at: r.get(14)?,
            ended_at: r.get(15)?,
            exit_code: r.get(16)?,
            timed_out: r.get::<_, i64>(17)? != 0,
            setup_ran: r.get::<_, i64>(18)? != 0,
            files_switched: r.get(19)?,
            log: r.get(20)?,
            output: r.get(21)?,
            error: r.get(22)?,
        },
        r.get(23)?,
    ))
}

impl Db {
    /// One run by id. A queued or running run whose process is gone reads as abandoned.
    pub fn lane_run(&self, id: i64) -> Result<Option<Run>> {
        let found = self.conn.query_row(&format!("SELECT {COLUMNS} FROM lane_run WHERE id = ?1"), [id], row).optional()?;
        Ok(found.map(|(run, pid)| settle(self, run, pid)))
    }

    /// A project's runs, newest first; queued and running ones whose process is gone are marked
    /// abandoned on the way.
    pub fn lane_runs(&self, project: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![project, limit as i64], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(run, pid)| settle(self, run, pid)).collect())
    }

    /// The runs one actor asked for, newest first.
    pub fn lane_runs_by(&self, by: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM lane_run WHERE requested_by = ?1 ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![by, limit as i64], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|(run, pid)| settle(self, run, pid)).collect())
    }
}

/// A queued or running run outlives the process serving it only by a crash: it is abandoned.
fn settle(db: &Db, mut run: Run, pid: Option<i64>) -> Run {
    let open = run.state == "queued" || run.state == "running";
    if open && pid.is_some_and(|p| p as u32 != std::process::id() && !crate::crew::pid_alive(p as u32)) {
        db.conn
            .execute("UPDATE lane_run SET state = 'abandoned', ended_at = COALESCE(ended_at, ?2) WHERE id = ?1 AND state IN ('queued', 'running')", params![run.id, now()])
            .ok();
        run.state = "abandoned".into();
    }
    run
}

/// Where a project's run logs go.
fn logs_dir(project: &str) -> PathBuf {
    crate::paths::data_dir().join("lanes").join(project.to_uppercase())
}

/// How long a snapshot's ref and a run's log are kept.
const KEEP_SECS: i64 = 7 * 24 * 3600;

/// `run` as the MCP tool and the CLI ask for it: the worktree `dir` is in, as a snapshot, in a lane
/// of the repo its issue belongs to. The issue is `issue`, else the one the branch names.
pub fn run_in(config: &crate::Config, db: &Db, dir: &str, issue: Option<&str>, command: &str, actor: &Actor, on_event: &mut dyn FnMut(&Run)) -> Result<Run> {
    if command.trim().is_empty() {
        bail!("say which command to run");
    }
    let (worktree, branch_key) = playbook::current_worktree(dir)?;
    let key = issue.map(str::to_string).or(branch_key);
    let record = match key.as_deref() {
        Some(k) => Some(db.issue(k)?.with_context(|| format!("no issue {k}"))?),
        None => None,
    };
    let root = match &record {
        Some(i) => playbook::repo_for_issue(config, i.project.as_deref(), dir)?,
        None => playbook::repo_root(config, dir).with_context(|| format!("{dir} is not inside a git repo"))?,
    };
    let project = record
        .as_ref()
        .and_then(|i| i.project.clone())
        .or_else(|| config.repo_for_dir(&root.to_string_lossy()).and_then(|r| r.project.clone()))
        .unwrap_or_default();
    let pb = Playbook::load_with(&root, config)?;
    let snap = snapshot(&worktree)?;
    let req = Request { project: &project, issue: record.as_ref().map(|i| i.key.as_str()), kind: Kind::Run, snapshot: &snap, command };
    run(db, &root, &pb, &req, actor, on_event)
}

/// Runs `req` in one of the repo's lanes, waiting for a free one, and returns its record. The record
/// is written as the request moves (queued, running, done), so another process can follow it with
/// `Db::lane_run`. `on_event` is called with the run when it is queued and when it starts.
pub fn run(db: &Db, repo_root: &Path, pb: &Playbook, req: &Request, actor: &Actor, on_event: &mut dyn FnMut(&Run)) -> Result<Run> {
    let Some(rules) = pb.lanes.clone() else {
        bail!("{} has no [lanes] in its playbook, so it has no build lanes; run the command in your worktree, or add [lanes] (docs/build-lanes.md)", repo_root.display());
    };
    let root = repo_root.to_string_lossy().into_owned();
    db.conn.execute(
        "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'local', ?10, ?11)",
        params![req.project.to_uppercase(), root, req.issue, actor.id(), req.kind.as_str(), req.snapshot.commit, req.snapshot.tree, req.snapshot.base, req.command, now(), std::process::id()],
    )?;
    let id = db.conn.last_insert_rowid();
    // A made snapshot exists only as an object: a ref keeps git's gc from taking it while it waits.
    if req.snapshot.made {
        git(repo_root, &["update-ref", &format!("refs/nitely/runs/{id}"), &req.snapshot.commit]).ok();
    }
    expire(db, repo_root, req.project);
    if let Some(r) = db.lane_run(id)? {
        on_event(&r);
    }

    let (n, _lock) = take_lane(repo_root, pb, &rules)?;
    let lane = lane_path(repo_root, pb, n);
    let logs = logs_dir(req.project);
    std::fs::create_dir_all(&logs)?;
    let log = logs.join(format!("{id}.log"));
    db.conn.execute(
        "UPDATE lane_run SET state = 'running', lane = ?2, started_at = ?3, log_path = ?4 WHERE id = ?1",
        params![id, n, now(), log.to_string_lossy()],
    )?;
    if let Some(r) = db.lane_run(id)? {
        on_event(&r);
    }

    let outcome = serve(repo_root, pb, &rules, &lane, &log, req);
    let ended = now();
    match outcome {
        Ok(o) => {
            db.conn.execute(
                "UPDATE lane_run SET state = 'done', ended_at = ?2, exit_code = ?3, timed_out = ?4, setup_ran = ?5, files_switched = ?6, output = ?7 WHERE id = ?1",
                params![id, ended, o.exit_code, o.timed_out, o.setup_ran, o.files_switched, o.output],
            )?;
        }
        Err(e) => {
            let why = format!("{e:#}");
            let output = std::fs::read_to_string(&log).map(|t| playbook::tail(&t)).unwrap_or_default();
            db.conn.execute(
                "UPDATE lane_run SET state = 'lane_error', ended_at = ?2, error = ?3, output = ?4 WHERE id = ?1",
                params![id, ended, why, output],
            )?;
        }
    }
    let done = db.lane_run(id)?.context("the run's record went missing")?;
    let said = match (&done.error, done.exit_code, done.timed_out) {
        (Some(e), _, _) => format!("Lane {n} could not run `{}`: {}", done.command, e.lines().next().unwrap_or("")),
        (None, _, true) => format!("`{}` ran past {} minutes in lane {n} and was stopped", done.command, rules.timeout_minutes),
        (None, Some(code), _) => format!("Ran `{}` in lane {n}: exit {code} after {} s", done.command, ended - done.started_at.unwrap_or(ended)),
        (None, None, _) => format!("Ran `{}` in lane {n}", done.command),
    };
    if let Some(key) = req.issue {
        db.log_event(actor, Some(key), Some(&lane.to_string_lossy()), "lane_run", &format!("{said} (run {id})")).ok();
    }
    Ok(done)
}

/// Deletes snapshot refs and logs older than `KEEP_SECS`. Best effort.
fn expire(db: &Db, repo_root: &Path, project: &str) {
    let cutoff = now() - KEEP_SECS;
    // This repo's runs only: its refs are the ones this can delete.
    let Ok(mut stmt) = db.conn.prepare("SELECT id, log_path FROM lane_run WHERE project = ?1 COLLATE NOCASE AND repo = ?2 AND queued_at < ?3 AND log_path IS NOT NULL") else { return };
    let repo = repo_root.to_string_lossy().into_owned();
    let old: Vec<(i64, String)> = stmt.query_map(params![project, repo, cutoff], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default();
    for (id, log) in old {
        std::fs::remove_file(&log).ok();
        git(repo_root, &["update-ref", "-d", &format!("refs/nitely/runs/{id}")]).ok();
        db.conn.execute("UPDATE lane_run SET log_path = NULL WHERE id = ?1", [id]).ok();
    }
}

/// A lane's lock: an OS file lock, so a process that dies lets go of it.
pub struct LaneLock(#[allow(dead_code)] std::fs::File);

fn lock_path(repo_root: &Path, pb: &Playbook, n: u32) -> PathBuf {
    dir(repo_root, pb).join(format!("lane-{n}.lock"))
}

fn try_lock(repo_root: &Path, pb: &Playbook, n: u32) -> Result<Option<LaneLock>> {
    let path = lock_path(repo_root, pb, n);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LaneLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// The first free lane, waiting until one is. Lanes are numbered from 1.
fn take_lane(repo_root: &Path, pb: &Playbook, rules: &LaneRules) -> Result<(u32, LaneLock)> {
    loop {
        for n in 1..=rules.count.max(1) {
            if let Some(lock) = try_lock(repo_root, pb, n)? {
                return Ok((n, lock));
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

struct Outcome {
    exit_code: Option<i32>,
    timed_out: bool,
    setup_ran: bool,
    files_switched: i64,
    output: String,
}

/// The four steps, in a lane this process holds.
fn serve(repo_root: &Path, pb: &Playbook, rules: &LaneRules, lane: &Path, log: &Path, req: &Request) -> Result<Outcome> {
    let base = pb.integration_branch();
    if git(repo_root, &["cat-file", "-e", &format!("{}^{{commit}}", req.snapshot.commit)]).is_err() {
        bail!("snapshot {} is missing from the repo (expired or cleaned up); make a new one", &req.snapshot.commit[..req.snapshot.commit.len().min(9)]);
    }
    let limit = Duration::from_secs(rules.timeout_minutes.max(1) * 60);
    let files_switched = if !lane.join(".git").exists() {
        create_lane(repo_root, lane, &req.snapshot.commit)?;
        git(lane, &["ls-files"]).map(|o| o.lines().count() as i64).unwrap_or(0)
    } else {
        stop_leftovers(lane);
        let switched = git(lane, &["diff", "--name-only", "HEAD", &req.snapshot.commit]).map(|o| o.lines().count() as i64).unwrap_or(0);
        switch(lane, &req.snapshot.commit, &rules.keep)?;
        switched
    };
    let setup_ran = ensure_setup(repo_root, pb, lane, base, log, limit)?;
    let (exit_code, timed_out) = run_logged(req.command, lane, log, limit)?;
    let output = std::fs::read_to_string(log).map(|t| playbook::tail(&t)).unwrap_or_default();
    Ok(Outcome { exit_code, timed_out, setup_ran, files_switched, output })
}

/// Makes lane `lane` as a worktree at `commit`; `ensure_setup` sets it up.
fn create_lane(repo_root: &Path, lane: &Path, commit: &str) -> Result<()> {
    std::fs::create_dir_all(lane.parent().unwrap())?;
    // A folder left by a lane whose registration is gone (a crash during creation) goes first.
    git(repo_root, &["worktree", "prune"]).ok();
    if lane.exists() {
        std::fs::remove_dir_all(lane).with_context(|| format!("removing the half-made lane {}", lane.display()))?;
    }
    git(repo_root, &["worktree", "add", "--detach", &lane.to_string_lossy(), commit])?;
    Ok(())
}

/// Runs the playbook's setup in `lane` when it has never passed there, or a dependency file differs
/// from the last time it passed; each command bounded by `limit`. The record of a passed setup is
/// written only after every command passed, so a setup that failed or was stopped runs again next
/// time. Returns whether setup ran.
fn ensure_setup(repo_root: &Path, pb: &Playbook, lane: &Path, base: &str, log: &Path, limit: Duration) -> Result<bool> {
    if pb.worktree.setup.is_empty() {
        return Ok(false);
    }
    let record = git_path(lane, "nitely-lane-setup").context("the lane has no git folder for its setup record")?;
    let now = playbook::setup_fingerprint(&pb.worktree, lane);
    // The record: the fingerprint, then the untracked paths setup made. Setup ran and still holds
    // only while both match: a clean that took what setup made (not in `keep`) means it runs again.
    if let Ok(last) = std::fs::read_to_string(&record) {
        let mut lines = last.lines();
        let same = lines.next().is_some_and(|f| f.trim() == now);
        if same && lines.filter(|p| !p.trim().is_empty()).all(|p| lane.join(p.trim()).exists()) {
            return Ok(false);
        }
    }
    std::fs::remove_file(&record).ok();
    for command in &pb.worktree.setup {
        let command = playbook::expand(command, repo_root, lane, base);
        // Run again on failure as the gate's commands are (`[gate] retries`): a setup that fails
        // once, on a busy machine, then passes is not the change's fault.
        let mut tries = 0;
        loop {
            match run_logged(&command, lane, log, limit)? {
                (Some(0), _) => break,
                (_, true) => bail!("setup ran past {} minutes in the lane and was stopped: {command}", limit.as_secs() / 60),
                (code, false) if tries >= pb.gate.retries => {
                    bail!("setup failed in the lane (exit {}): {command}", code.map_or("none".to_string(), |c| c.to_string()))
                }
                _ => tries += 1,
            }
        }
    }
    // What setup left that git does not track, by its top-level path: `node_modules`, `.venv`.
    let status = git(lane, &["status", "--porcelain=v1", "-z", "--ignored=matching", "--untracked-files=normal"]).unwrap_or_default();
    let mut made: Vec<String> = status
        .split('\0')
        .filter_map(|e| e.strip_prefix("!! ").or_else(|| e.strip_prefix("?? ")))
        // The whole path git names (`packages/app/node_modules`), not its first part, which tracked
        // files would keep present after a clean took what setup made.
        .map(|p| p.trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    made.sort();
    made.dedup();
    std::fs::write(&record, format!("{now}\n{}", made.join("\n"))).with_context(|| format!("writing {}", record.display()))?;
    Ok(true)
}

/// A file in `dir`'s own git folder (per worktree), named `name`.
fn git_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = git(dir, &["rev-parse", "--git-path", name]).ok()?;
    let path = PathBuf::from(path.trim());
    Some(if path.is_absolute() { path } else { dir.join(path) })
}

/// Where a lane keeps the pid of the command running in it: in its own git folder, gone once the
/// command ends. Found at the start of a request, it means the last one's process died with its
/// command still running.
fn pid_file(lane: &Path) -> Option<PathBuf> {
    git_path(lane, "nitely-lane-pid")
}

/// Stops what a request that died left running in `lane`: the command it recorded, with what that
/// started, and any process whose executable or command line is inside the lane (test binaries
/// under its build directory). Only when the last request did not end normally.
fn stop_leftovers(lane: &Path) {
    let Some(file) = pid_file(lane) else { return };
    let Ok(text) = std::fs::read_to_string(&file) else { return };
    // "<pid>\n<command>": the pid is stopped only while it still runs that command, since the
    // system may have given the number to another process once the command ended.
    let (pid, command) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    if let Ok(pid) = pid.trim().parse::<u32>() {
        let same = !command.trim().is_empty() && crate::procs::command_line(pid).is_some_and(|line| line.contains(command.trim()));
        if same {
            stop_tree(pid);
        }
    }
    for h in crate::procs::holders(lane) {
        stop_tree(h.pid);
    }
    std::fs::remove_file(&file).ok();
}

/// Puts `lane` at `commit`: a forced checkout, then everything untracked or ignored removed except
/// the `keep` directories. One retry after a short wait, for a file a finished process had open.
fn switch(lane: &Path, commit: &str, keep: &[String]) -> Result<()> {
    let attempt = || -> Result<()> {
        git(lane, &["checkout", "--quiet", "--force", "--detach", commit])?;
        let mut args: Vec<String> = vec!["clean".into(), "-ffdxq".into()];
        for k in keep {
            args.push("-e".into());
            args.push(format!("/{}", k.trim_matches('/')));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(lane, &args)?;
        Ok(())
    };
    attempt().or_else(|first| {
        std::thread::sleep(Duration::from_secs(2));
        attempt().map_err(|e| e.context(format!("switching the lane failed twice (first: {first:#})")))
    })
}

/// Runs `command` in `dir` with its output appended to `log`, stopping it and everything it started
/// after `limit`. Returns its exit code (None when stopped) and whether it timed out.
fn run_logged(command: &str, dir: &Path, log: &Path, limit: Duration) -> Result<(Option<i32>, bool)> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "$ {command}")?;
    let mut cmd = playbook::shell_command(command);
    cmd.current_dir(dir).stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // In a lane, the pid is kept while it runs, so a request after a crash can stop it. The file is
    // written before the start too, without a pid, so a crash in between still leaves the mark that
    // makes the next request look for what runs in the lane.
    let pids = pid_file(dir);
    if let Some(p) = &pids {
        std::fs::write(p, format!("starting\n{command}")).ok();
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(p) = &pids {
                std::fs::remove_file(p).ok();
            }
            return Err(e).with_context(|| format!("starting {command}"));
        }
    };
    if let Some(p) = &pids {
        std::fs::write(p, format!("{}\n{command}", child.id())).ok();
    }
    let started = Instant::now();
    let result = loop {
        if let Some(status) = child.try_wait()? {
            break (status.code(), false);
        }
        if started.elapsed() > limit {
            stop_tree(child.id());
            child.kill().ok();
            child.wait().ok();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if let Some(p) = &pids {
        std::fs::remove_file(p).ok();
    }
    Ok(result)
}

/// Stops `pid` and everything it started.
fn stop_tree(pid: u32) {
    let pid = pid.to_string();
    #[cfg(windows)]
    {
        playbook::shell_free_command("taskkill").args(["/T", "/F", "/PID", &pid]).output().ok();
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).output().ok();
    }
}

// ----- what `nitely lanes` shows and does -----

#[derive(Debug, Clone, Serialize)]
pub struct LaneState {
    pub lane: u32,
    pub path: String,
    /// The lane's checkout exists (it is made on its first request).
    pub made: bool,
    /// A request holds it now.
    pub busy: bool,
    /// The commit it holds.
    pub at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lanes {
    pub project: String,
    pub repo: String,
    pub dir: String,
    pub rules: LaneRules,
    pub lanes: Vec<LaneState>,
    /// Waiting and running requests, oldest first.
    pub queue: Vec<Run>,
    /// The last finished runs, newest first.
    pub recent: Vec<Run>,
}

/// The repo's lanes, its queue and its last runs. None when its playbook has no `[lanes]`.
pub fn status(db: &Db, repo_root: &Path, pb: &Playbook, project: &str) -> Result<Option<Lanes>> {
    let Some(rules) = pb.lanes.clone() else { return Ok(None) };
    let mut lanes = Vec::new();
    for n in 1..=rules.count.max(1) {
        let path = lane_path(repo_root, pb, n);
        let made = path.join(".git").exists();
        let busy = try_lock(repo_root, pb, n)?.is_none();
        let at = made.then(|| git(&path, &["rev-parse", "--short", "HEAD"]).ok().map(|s| s.trim().to_string())).flatten();
        lanes.push(LaneState { lane: n, path: path.to_string_lossy().into_owned(), made, busy, at });
    }
    let runs = db.lane_runs(project, 50)?;
    let root = repo_root.to_string_lossy();
    let mine = |r: &&Run| crate::paths::normalize(&r.repo) == crate::paths::normalize(&root);
    let mut queue: Vec<Run> = runs.iter().filter(mine).filter(|r| r.state == "queued" || r.state == "running").cloned().collect();
    queue.reverse();
    let recent = runs.iter().filter(mine).filter(|r| r.state != "queued" && r.state != "running").take(10).cloned().collect();
    Ok(Some(Lanes { project: project.to_uppercase(), repo: root.into_owned(), dir: dir(repo_root, pb).to_string_lossy().into_owned(), rules, lanes, queue, recent }))
}

/// Puts every lane that is free back to a clean checkout of what it holds; its build output stays.
/// Returns what it did per lane, in words.
pub fn reset(repo_root: &Path, pb: &Playbook) -> Result<Vec<String>> {
    let Some(rules) = pb.lanes.clone() else { bail!("{} has no [lanes] in its playbook", repo_root.display()) };
    let mut said = Vec::new();
    for n in 1..=rules.count.max(1) {
        let path = lane_path(repo_root, pb, n);
        if !path.join(".git").exists() {
            said.push(format!("lane {n}: not made yet"));
            continue;
        }
        let Some(_lock) = try_lock(repo_root, pb, n)? else {
            said.push(format!("lane {n}: busy with a request, left alone"));
            continue;
        };
        let head = git(&path, &["rev-parse", "HEAD"])?.trim().to_string();
        match switch(&path, &head, &rules.keep) {
            Ok(()) => said.push(format!("lane {n}: clean at {}", &head[..head.len().min(9)])),
            Err(e) => said.push(format!("lane {n}: {e:#}")),
        }
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn repo(base: &Path) -> PathBuf {
        let root = base.join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let g = |args: &[&str]| assert!(playbook::run_git(Command::new("git").arg("-C").arg(&root).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("src/a.txt"), "one\n").unwrap();
        std::fs::write(root.join("src/b.txt"), "two\n").unwrap();
        std::fs::write(root.join(".gitignore"), "/out/\n/scratch/\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        root
    }

    fn playbook_with(text: &str) -> Playbook {
        Playbook::parse(text).unwrap()
    }

    #[test]
    fn changed_paths_reads_porcelain_v2() {
        let status = ["1 .M N... 100644 100644 100644 abc abc src/a.txt", "? src/new file.txt", "2 R. N... 100644 100644 100644 abc abc R100 src/c.txt", "src/b.txt", ""].join("\0");
        assert_eq!(changed_paths(&status), ["src/a.txt", "src/b.txt", "src/c.txt", "src/new file.txt"]);
    }

    #[test]
    fn a_snapshot_is_head_or_a_commit_of_the_worktree_without_touching_it() {
        let base = crate::paths::unique_temp("nitely-snap", "");
        let root = repo(&base);
        let head = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let clean = snapshot(&root).unwrap();
        assert_eq!((clean.commit.as_str(), clean.made), (head.as_str(), false));

        std::fs::write(root.join("src/a.txt"), "one, changed\n").unwrap();
        std::fs::remove_file(root.join("src/b.txt")).unwrap();
        std::fs::write(root.join("src/c.txt"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/build.bin"), "ignored").unwrap();
        let before = git(&root, &["status", "--porcelain"]).unwrap();
        let snap = snapshot(&root).unwrap();
        assert!(snap.made && snap.base == head);
        assert_eq!(git(&root, &["status", "--porcelain"]).unwrap(), before, "the worktree and its index are as they were");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap().trim(), head, "and its branch");
        let files = git(&root, &["ls-tree", "-r", "--name-only", &snap.commit]).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "src/a.txt", "src/c.txt"], "ignored files stay out");
        assert_eq!(git(&root, &["show", &format!("{}:src/a.txt", snap.commit)]).unwrap(), "one, changed\n");

        // A tracked folder that became a file.
        std::fs::remove_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src"), "now a file\n").unwrap();
        let snap = snapshot(&root).unwrap();
        let files = git(&root, &["ls-tree", "-r", "--name-only", &snap.commit]).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "src"]);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_lane_runs_requests_keeping_only_its_build_dirs() {
        let base = crate::paths::unique_temp("nitely-lane", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        // No [lanes]: no lanes, and the caller is told what to do instead.
        let none = playbook_with("[gate]\ncommands = [\"x\"]\n");
        let snap = snapshot(&root).unwrap();
        let req = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap, command: "echo hi" };
        assert!(run(&db, &root, &none, &req, &actor, &mut |_| {}).unwrap_err().to_string().contains("no [lanes]"));
        assert!(status(&db, &root, &none, "APP").unwrap().is_none());

        let pb = playbook_with("[lanes]\nkeep = [\"out\"]\ntimeout_minutes = 1\n");
        let lister = if cfg!(windows) { "dir /b src && (if not exist out mkdir out) && echo built > out\\done.txt && echo junk > scratch.txt" } else { "ls src && mkdir -p out && echo built > out/done.txt && echo junk > scratch.txt" };
        let req = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap, command: lister };
        let mut seen = Vec::new();
        let first = run(&db, &root, &pb, &req, &actor, &mut |r| seen.push(r.state.clone())).unwrap();
        assert_eq!(seen, ["queued", "running"]);
        assert!(first.passed(), "{first:?}");
        assert_eq!(first.lane, Some(1));
        assert!(first.output.contains("a.txt") && first.output.contains("b.txt"), "{}", first.output);
        let lane = lane_path(&root, &pb, 1);
        assert!(lane.join("out/done.txt").exists() && lane.join("scratch.txt").exists());

        // The next request: another snapshot; the build dir stays, the stray file and b.txt go.
        std::fs::remove_file(root.join("src/b.txt")).unwrap();
        let snap2 = snapshot(&root).unwrap();
        let check = if cfg!(windows) { "if exist scratch.txt exit 3 & if not exist out\\done.txt exit 4 & if exist src\\b.txt exit 5" } else { "test ! -e scratch.txt || exit 3; test -e out/done.txt || exit 4; test ! -e src/b.txt || exit 5" };
        let req2 = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap2, command: check };
        let second = run(&db, &root, &pb, &req2, &actor, &mut |_| {}).unwrap();
        assert_eq!(second.exit_code, Some(0), "{second:?}");
        assert_eq!(second.files_switched, Some(1));

        // A failing command is a result, not a lane problem; the timeline names the run.
        let req3 = Request { project: "APP", issue: Some("APP-1"), kind: Kind::Run, snapshot: &snap2, command: "exit 7" };
        let third = run(&db, &root, &pb, &req3, &actor, &mut |_| {}).unwrap();
        assert_eq!((third.state.as_str(), third.exit_code, third.passed()), ("done", Some(7), false));
        let said: Vec<String> = db.timeline("APP-1").unwrap().into_iter().filter(|t| t.kind == "lane_run").filter_map(|t| t.summary).collect();
        assert_eq!(said.len(), 2);
        assert!(said[1].contains("exit 7") && said[1].contains(&format!("run {}", third.id)), "{said:?}");

        let lanes = status(&db, &root, &pb, "APP").unwrap().unwrap();
        assert_eq!(lanes.lanes.len(), 1);
        assert!(lanes.lanes[0].made && !lanes.lanes[0].busy);
        assert!(lanes.queue.is_empty() && lanes.recent.len() == 3);
        assert_eq!(reset(&root, &pb).unwrap().len(), 1);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_busy_lane_waits_and_a_dead_requester_is_abandoned() {
        let base = crate::paths::unique_temp("nitely-lane-busy", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]\ncount = 1\n");
        let held = try_lock(&root, &pb, 1).unwrap().expect("free at first");
        assert!(try_lock(&root, &pb, 1).unwrap().is_none(), "held by this handle");
        drop(held);
        assert!(try_lock(&root, &pb, 1).unwrap().is_some(), "free once dropped, as after a crash");

        // A row left queued by a process that is gone reads as abandoned.
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid) VALUES ('APP', ?1, 'x', 'run', 's', 't', 'b', 'c', 'queued', 'local', 1, 999999)",
                [root.to_string_lossy()],
            )
            .unwrap();
        let id = db.conn.last_insert_rowid();
        assert_eq!(db.lane_run(id).unwrap().unwrap().state, "abandoned");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_setup_that_failed_runs_again_and_a_dead_requests_command_is_stopped() {
        let base = crate::paths::unique_temp("nitely-lane-setup", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        // Fails the first time it runs in a lane (it leaves a mark outside the repo), passes after.
        let mark = base.join("setup-ran-once");
        let mark_s = mark.to_string_lossy().replace('\\', "/");
        let setup = if cfg!(windows) { format!("if exist \"{mark_s}\" (exit 0) else (type nul > \"{mark_s}\" & exit 1)") } else { format!("if [ -e '{mark_s}' ]; then exit 0; else touch '{mark_s}'; exit 1; fi") };
        // No retries, so the first request sees the failure (with the default one retry it passes).
        let pb = playbook_with(&format!("[worktree]\nsetup = [{setup:?}]\n[gate]\nretries = 0\n[lanes]\n"));
        let snap = snapshot(&root).unwrap();
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: "echo built" };
        let first = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert_eq!(first.state, "lane_error", "{first:?}");
        assert!(first.error.as_deref().unwrap_or("").contains("setup failed"), "{first:?}");
        let second = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert!(second.passed() && second.setup_ran, "a failed setup is never taken as done: {second:?}");
        let third = run(&db, &root, &pb, &req, &actor, &mut |_| {}).unwrap();
        assert!(third.passed() && !third.setup_ran, "and a passed one is not run again");

        // What setup makes and `keep` does not list goes with the clean, so setup runs again; what
        // `keep` lists stays, and setup does not. Inside a tracked folder (`src/deps`), which the
        // clean leaves in place.
        let makes = if cfg!(windows) { "if not exist src\\deps mkdir src\\deps && echo x > src\\deps\\ok.txt" } else { "mkdir -p src/deps && echo x > src/deps/ok.txt" };
        let needs = if cfg!(windows) { "if not exist src\\deps\\ok.txt exit 9" } else { "test -e src/deps/ok.txt || exit 9" };
        let other = crate::paths::unique_temp("nitely-lane-keep", "");
        let root2 = repo(&other);
        let snap2 = snapshot(&root2).unwrap();
        for (keep, again) in [("", true), ("keep = [\"src/deps\"]\n", false)] {
            let pb2 = playbook_with(&format!("[worktree]\nsetup = [{makes:?}]\n[lanes]\n{keep}"));
            let req2 = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap2, command: needs };
            assert!(run(&db, &root2, &pb2, &req2, &actor, &mut |_| {}).unwrap().passed());
            let next = run(&db, &root2, &pb2, &req2, &actor, &mut |_| {}).unwrap();
            assert!(next.passed(), "{next:?}");
            assert_eq!(next.setup_ran, again, "keep {keep:?}");
        }
        std::fs::remove_dir_all(other).ok();

        // A command left running by a request whose process died is stopped before the next one.
        let lane = lane_path(&root, &pb, 1);
        let sleeper = if cfg!(windows) { "ping -n 60 127.0.0.1 >NUL" } else { "sleep 60" };
        let mut left = playbook::shell_command(sleeper);
        left.current_dir(&lane);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            left.process_group(0);
        }
        // A recorded pid now running something else (the number was reused) is left alone.
        std::fs::write(pid_file(&lane).unwrap(), format!("{}\nsome other command", std::process::id())).unwrap();
        stop_leftovers(&lane);
        assert!(!pid_file(&lane).unwrap().exists(), "this test process is still here to check");
        let mut child = left.spawn().unwrap();
        std::fs::write(pid_file(&lane).unwrap(), format!("{}\n{sleeper}", child.id())).unwrap();
        stop_leftovers(&lane);
        let stopped = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            matches!(child.try_wait(), Ok(Some(_)))
        });
        assert!(stopped, "the leftover command was stopped");
        assert!(!pid_file(&lane).unwrap().exists());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_command_past_its_time_is_stopped() {
        let base = crate::paths::unique_temp("nitely-lane-time", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        let sleeper = if cfg!(windows) { "ping -n 30 127.0.0.1 >NUL" } else { "sleep 30" };
        let started = Instant::now();
        let (code, timed_out) = run_logged(sleeper, &base, &log, Duration::from_secs(1)).unwrap();
        assert!(timed_out && code.is_none());
        assert!(started.elapsed() < Duration::from_secs(20), "stopped, not waited out");
        std::fs::remove_dir_all(base).ok();
    }
}
