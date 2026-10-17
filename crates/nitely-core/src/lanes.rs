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
    /// Commands (regexes, matched against each command of a shell line) that build or test: run in
    /// a light worktree, the hook reminds the agent to use `run` instead (it never blocks them).
    pub heavy: Vec<String>,
    /// How big a lane's build directories may grow, in GB, before `hygiene` trims them; unset, twice
    /// the size of its first build.
    pub size_cap_gb: Option<f64>,
}

/// The commands that build or test when the playbook names none.
pub const DEFAULT_HEAVY: &[&str] = &["^cargo (build|test|clippy|check|run)\\b", "^npm (ci|install|test|run build)\\b"];

impl Default for LaneRules {
    fn default() -> Self {
        LaneRules {
            count: 1,
            keep: crate::slim::DEFAULT_KEEP.iter().map(|s| s.to_string()).collect(),
            timeout_minutes: 30,
            heavy: DEFAULT_HEAVY.iter().map(|s| s.to_string()).collect(),
            size_cap_gb: None,
        }
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

/// The folder for what lanes keep outside the repo (run logs): `lanes` beside the database the runs
/// are recorded in, so a database and its runs' logs stay together (the data folder, or a test's
/// own folder).
pub fn state_dir(db: &Db) -> PathBuf {
    let beside = db.conn.path().filter(|p| !p.is_empty()).and_then(|p| Path::new(p).parent().map(Path::to_path_buf));
    beside.unwrap_or_else(crate::paths::data_dir).join("lanes")
}

/// Where a project's run logs go.
fn logs_dir(db: &Db, project: &str) -> PathBuf {
    let project = if project.is_empty() { "_" } else { project };
    state_dir(db).join(project.to_uppercase())
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
    let jobs = machine_budget().jobs;
    let command = req.command.replace("{jobs}", &jobs.to_string());
    run_with(db, repo_root, pb, req, actor, on_event, &mut |lane, log, limit| run_logged(&command, lane, log, limit, jobs))
}

/// What runs in a lane once it holds the request's snapshot: given the lane, the log and the time
/// limit, it returns the exit code (None when stopped) and whether it ran past the limit.
type Exec<'a> = dyn FnMut(&Path, &Path, Duration) -> Result<(Option<i32>, bool)> + 'a;

/// `run`, with what happens in the lane given by `exec` (one command, or a gate's commands).
fn run_with(db: &Db, repo_root: &Path, pb: &Playbook, req: &Request, actor: &Actor, on_event: &mut dyn FnMut(&Run), exec: &mut Exec) -> Result<Run> {
    let Some(rules) = pb.lanes.clone() else {
        bail!("{} has no [lanes] in its playbook, so it has no build lanes; run the command in your worktree, or add [lanes] (docs/build-lanes.md)", repo_root.display());
    };
    let root = repo_root.to_string_lossy().into_owned();
    // Land first, then submit's gate, then everything else (the scheduler, NIT-199, orders by it).
    let priority = match req.kind {
        Kind::Land => 1,
        Kind::Gate => 2,
        Kind::Run => 3,
    };
    db.conn.execute(
        "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'local', ?10, ?11, ?12)",
        params![req.project.to_uppercase(), root, req.issue, actor.id(), req.kind.as_str(), req.snapshot.commit, req.snapshot.tree, req.snapshot.base, req.command, now(), std::process::id(), priority],
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

    // Until it runs, a failure here (a lock that cannot be opened, a log folder that cannot be made)
    // closes the request's row: a queued row left behind would hold up the requests after it.
    let started = (|| -> Result<_> {
        let (n, lock, slot) = take_turn(db, id, repo_root, pb, &rules, machine_budget().slots)?;
        let logs = logs_dir(db, req.project);
        std::fs::create_dir_all(&logs)?;
        let log = logs.join(format!("{id}.log"));
        db.conn.execute(
            "UPDATE lane_run SET state = 'running', lane = ?2, started_at = ?3, log_path = ?4 WHERE id = ?1",
            params![id, n, now(), log.to_string_lossy()],
        )?;
        Ok((n, lock, slot, log))
    })();
    let (n, _lock, _slot, log) = match started {
        Ok(s) => s,
        Err(e) => {
            db.conn
                .execute(
                    "UPDATE lane_run SET state = 'lane_error', ended_at = ?2, error = ?3 WHERE id = ?1 AND state IN ('queued', 'running')",
                    params![id, now(), format!("{e:#}")],
                )
                .ok();
            return Err(e);
        }
    };
    let lane = lane_path(repo_root, pb, n);
    if let Some(r) = db.lane_run(id)? {
        on_event(&r);
    }

    let outcome = serve(repo_root, pb, &rules, &lane, &log, req, exec);
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

/// A gate that ran in a lane (or was answered from one), for `playbook::run_gate` to record the way
/// it records a gate in a worktree.
pub struct GateRun {
    /// The snapshot's short commit.
    pub commit: String,
    pub results: Vec<playbook::CommandResult>,
    pub passed: bool,
    pub flaked: Vec<playbook::Flake>,
    /// The lane run it was, or the earlier one it was answered from.
    pub run: Option<i64>,
    /// What the caller should know: uncommitted work left out of the gate.
    pub warnings: Vec<String>,
}

/// The gate's `steps` (command, why it is skipped) for `worktree`, as one lane request: one switch
/// to the worktree's snapshot, then each command that is not skipped, in order, each run again up to
/// `[gate] retries` times when it fails (a pass after a failure is a flake), stopping at the first
/// that keeps failing. A tree whose same commands already passed in a lane is answered from that
/// run's record without running anything.
#[allow(clippy::too_many_arguments)]
pub fn gate(db: &Db, repo_root: &Path, pb: &Playbook, worktree: &Path, issue: Option<&str>, kind: Kind, steps: &[(String, Option<String>)], actor: &Actor) -> Result<GateRun> {
    // What is committed, as land will build it: the branch head, never a commit made for the
    // request, so the gate reports the commit the review sees.
    let top = PathBuf::from(git(worktree, &["rev-parse", "--show-toplevel"])?.trim());
    let head = git(&top, &["rev-parse", "HEAD"])?.trim().to_string();
    let snap = Snapshot { tree: git(&top, &["rev-parse", "HEAD^{tree}"])?.trim().to_string(), commit: head.clone(), base: head, made: false };
    let mut warnings = Vec::new();
    if snapshot(worktree)?.made {
        warnings.push("the worktree has uncommitted work, which this gate leaves out: it builds what is committed, as land will; `run` builds the worktree as it is".to_string());
    }
    let short = snap.commit[..snap.commit.len().min(7)].to_string();
    let project = issue.and_then(|k| db.issue(k).ok().flatten()).and_then(|i| i.project).unwrap_or_default();
    let base = pb.integration_branch().to_string();
    let skipped = |why: &str, command: &str| playbook::CommandResult { command: command.to_string(), ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true };
    // The commands as the playbook writes them: the record's command, and what an earlier pass of
    // the same tree must have run.
    let to_run: Vec<&str> = steps.iter().filter(|(_, skip)| skip.is_none()).map(|(c, _)| c.as_str()).collect();
    let key = to_run.join("\n");
    let root = repo_root.to_string_lossy().into_owned();
    // One command per line is the key, so a command that holds a line break of its own could
    // stand for two: such a gate is never answered from a record.
    let keyed = !to_run.iter().any(|c| c.contains('\n'));
    let passed_before: Option<i64> = if !keyed {
        None
    } else {
        db.conn
            .query_row(
                "SELECT id FROM lane_run WHERE repo = ?1 AND tree = ?2 AND command = ?3 AND kind IN ('gate', 'land') AND state = 'done' AND exit_code = 0 ORDER BY id DESC LIMIT 1",
                params![root, snap.tree, key],
                |r| r.get(0),
            )
            .optional()?
    };
    if let Some(id) = passed_before {
        let results = steps
            .iter()
            .map(|(command, skip)| match skip {
                Some(why) => skipped(why, command),
                None => skipped(&format!("this tree passed it in lane run {id}; not run again"), command),
            })
            .collect();
        if let Some(key) = issue {
            db.log_event(actor, Some(key), None, "lane_run", &format!("The gate for {short} passed in lane run {id} already; not run again")).ok();
        }
        return Ok(GateRun { commit: short, results, passed: true, flaked: vec![], run: Some(id), warnings });
    }

    let mut results: Vec<playbook::CommandResult> = Vec::new();
    let mut flaked: Vec<playbook::Flake> = Vec::new();
    let retries = pb.gate.retries;
    let jobs = machine_budget().jobs;
    let req = Request { project: &project, issue, kind, snapshot: &snap, command: &key };
    let run = run_with(db, repo_root, pb, &req, actor, &mut |_| {}, &mut |lane: &Path, log: &Path, limit: Duration| {
        for (command, skip) in steps {
            if let Some(why) = skip {
                results.push(skipped(why, command));
                continue;
            }
            let command = playbook::expand(command, repo_root, lane, &base).replace("{jobs}", &jobs.to_string());
            let started = Instant::now();
            let mut failures: Vec<String> = Vec::new();
            loop {
                let from = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
                let (code, timed_out) = run_logged(&command, lane, log, limit, jobs)?;
                let output = log_since(log, from);
                let seconds = started.elapsed().as_secs();
                if timed_out {
                    results.push(playbook::CommandResult { command: command.clone(), ok: false, seconds, output, skipped: false });
                    return Ok((None, true));
                }
                if code == Some(0) {
                    if !failures.is_empty() {
                        let tests = failures.iter().flat_map(|o| playbook::failed_tests(o)).fold(Vec::<String>::new(), |mut all, t| {
                            if !all.contains(&t) {
                                all.push(t);
                            }
                            all
                        });
                        flaked.push(playbook::Flake { command: command.clone(), tests, output: failures.swap_remove(0) });
                    }
                    results.push(playbook::CommandResult { command: command.clone(), ok: true, seconds, output, skipped: false });
                    break;
                }
                if failures.len() as u32 >= retries {
                    results.push(playbook::CommandResult { command: command.clone(), ok: false, seconds, output, skipped: false });
                    return Ok((code, false));
                }
                failures.push(output);
            }
        }
        Ok((Some(0), false))
    })?;
    if let Some(why) = &run.error {
        // The setup's own output too, so the caller sees why it failed.
        let output = if run.output.trim().is_empty() { why.clone() } else { format!("{why}\n{}", run.output) };
        results.push(playbook::CommandResult { command: format!("lane {}", run.lane.unwrap_or(0)), ok: false, seconds: 0, output, skipped: false });
    }
    Ok(GateRun { commit: short, passed: run.passed(), results, flaked, run: Some(run.id), warnings })
}

/// What `log` gained after byte `from`, its end kept as a command's output is.
fn log_since(log: &Path, from: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut text = Vec::new();
    if let Ok(mut f) = std::fs::File::open(log) {
        if f.seek(SeekFrom::Start(from)).is_ok() {
            f.read_to_end(&mut text).ok();
        }
    }
    playbook::tail(&String::from_utf8_lossy(&text))
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

// ----- the machine's build budget and its queue (NIT-199) -----

/// How many lane requests this machine runs at once across every project, and the jobs each gets.
#[derive(Debug, Clone, Serialize)]
pub struct Budget {
    pub slots: u32,
    /// Cores for one request: the machine's cores shared by its slots.
    pub jobs: u32,
    pub cores: u32,
    pub memory_gb: Option<u64>,
    /// Where `slots` comes from: `machine` (cores and memory), `settings` (`build_slots` in
    /// Nitely's config) or `NITELY_BUILD_SLOTS`.
    pub from: String,
}

/// Cores per slot, and memory per slot, when the machine decides.
const CORES_PER_SLOT: u32 = 8;
const GB_PER_SLOT: u64 = 8;

/// The budget for `cores` and `memory_gb`, unless `set` says how many slots.
pub fn budget_for(cores: u32, memory_gb: Option<u64>, set: Option<(u32, &str)>) -> Budget {
    let cores = cores.max(1);
    let (slots, from) = match set.filter(|(n, _)| *n > 0) {
        Some((n, from)) => (n, from.to_string()),
        None => {
            let by_cores = cores / CORES_PER_SLOT;
            let by_memory = memory_gb.map(|m| (m / GB_PER_SLOT) as u32).unwrap_or(by_cores);
            (by_cores.min(by_memory).max(1), "machine".to_string())
        }
    };
    Budget { slots, jobs: (cores / slots).max(1), cores, memory_gb, from }
}

/// This machine's budget: `NITELY_BUILD_SLOTS`, else `build_slots` in Nitely's config, else from
/// its cores and memory.
pub fn machine_budget() -> Budget {
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let env = std::env::var("NITELY_BUILD_SLOTS").ok().and_then(|v| v.trim().parse::<u32>().ok());
    let settings = crate::Config::load_read_only().ok().and_then(|c| c.build_slots);
    let set = env.map(|n| (n, "NITELY_BUILD_SLOTS")).or(settings.map(|n| (n, "settings")));
    budget_for(cores, memory_gb(), set)
}

/// The machine's memory in GB, asked once per process. None when it cannot be told.
fn memory_gb() -> Option<u64> {
    static MEMORY: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MEMORY.get_or_init(|| {
        let bytes: Option<u64> = if cfg!(windows) {
            playbook::shell_free_command("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        } else if cfg!(target_os = "macos") {
            std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        } else {
            std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| {
                let kb = t.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse::<u64>().ok()?;
                Some(kb * 1024)
            })
        };
        // Rounded, so 63.9 GB of usable memory counts as the 64 it is sold as.
        bytes.map(|b| (b + (1 << 29)) >> 30)
    })
}

/// A machine slot's lock: an OS file lock beside the database, so a process that dies frees it.
pub struct SlotLock(#[allow(dead_code)] std::fs::File);

fn try_slot(db: &Db, k: u32) -> Result<Option<SlotLock>> {
    let dir = state_dir(db);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("slot-{k}.lock"));
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(SlotLock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// A queued request as the queue orders them.
#[derive(Debug, Clone)]
struct Waiting {
    id: i64,
    project: String,
    repo: String,
    priority: i64,
}

/// The order requests start in: priority first (land, then submit's gate, then the rest), then
/// each project's first waiting request before any project's second, then the oldest.
/// Among requests at the same place in their projects' queues, the project served longest ago
/// goes first (`last_started`, by upper-case project; never served counts as longest ago), so a
/// project whose request just started or ran waits behind the others' next, whenever its own was
/// queued.
fn in_order(mut waiting: Vec<Waiting>, last_started: &std::collections::HashMap<String, i64>) -> Vec<Waiting> {
    waiting.sort_by_key(|w| (w.priority, w.id));
    let mut seen: std::collections::HashMap<(i64, String), usize> = std::collections::HashMap::new();
    let mut ranked: Vec<(i64, usize, i64, i64, Waiting)> = waiting
        .into_iter()
        .map(|w| {
            let project = w.project.to_uppercase();
            let n = seen.entry((w.priority, project.clone())).or_default();
            *n += 1;
            let last = last_started.get(&project).copied().unwrap_or(i64::MIN);
            (w.priority, *n, last, w.id, w)
        })
        .collect();
    ranked.sort_by_key(|(p, n, last, id, _)| (*p, *n, *last, *id));
    ranked.into_iter().map(|(_, _, _, _, w)| w).collect()
}

/// Whether request `id` may try for a slot and a lane now: no request of its own repo is ahead of
/// it, and fewer requests ahead of it, of repos with a free lane, than there are free slots. A
/// repo whose lanes are all busy does not hold up the others.
fn my_turn(db: &Db, id: i64, slots: u32, alive: &mut dyn FnMut(i64) -> bool) -> Result<bool> {
    let mut stmt = db.conn.prepare("SELECT id, project, repo, priority, state, pid, lane_count, queued_at FROM lane_run WHERE state IN ('queued', 'running')")?;
    let rows: Vec<(Waiting, String, Option<i64>, i64, i64)> = stmt
        .query_map([], |r| Ok((Waiting { id: r.get(0)?, project: r.get(1)?, repo: r.get(2)?, priority: r.get(3)? }, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut running_by_repo: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut lanes_by_repo: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut running = 0i64;
    let mut waiting = Vec::new();
    for (w, state, pid, lane_count, _queued_at) in rows {
        let repo = crate::paths::normalize(&w.repo);
        lanes_by_repo.insert(repo.clone(), lane_count.max(1));
        // A request whose process is gone holds nothing and waits for nothing.
        if w.id != id && pid.is_some_and(|p| !alive(p)) {
            continue;
        }
        if state == "running" {
            running += 1;
            *running_by_repo.entry(repo).or_default() += 1;
        } else {
            waiting.push(w);
        }
    }
    // When each project last had a turn (a request of it started), so the one served longest ago
    // goes first and one project's backlog never holds up the rest.
    let mut served: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut stmt = db.conn.prepare("SELECT project, MAX(started_at) FROM lane_run WHERE started_at IS NOT NULL GROUP BY project")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (project, at) = row?;
        let slot = served.entry(project.to_uppercase()).or_insert(at);
        *slot = (*slot).max(at);
    }
    let free_slots = (slots as i64 - running).max(0);
    let mine = crate::paths::normalize(&waiting.iter().find(|w| w.id == id).map(|w| w.repo.clone()).unwrap_or_default());
    let mut ahead_that_can_start = 0;
    for w in in_order(waiting, &served) {
        if w.id == id {
            return Ok(ahead_that_can_start < free_slots);
        }
        let repo = crate::paths::normalize(&w.repo);
        if repo == mine {
            return Ok(false);
        }
        let free_lanes = lanes_by_repo.get(&repo).copied().unwrap_or(1) - running_by_repo.get(&repo).copied().unwrap_or(0);
        if free_lanes > 0 {
            ahead_that_can_start += 1;
        }
    }
    // Not queued (any more): nothing to wait for.
    Ok(true)
}

/// Waits for request `id`'s turn, then holds a machine slot and one of the repo's lanes.
fn take_turn(db: &Db, id: i64, repo_root: &Path, pb: &Playbook, rules: &LaneRules, slots: u32) -> Result<(u32, LaneLock, SlotLock)> {
    db.conn.execute("UPDATE lane_run SET lane_count = ?2 WHERE id = ?1", params![id, rules.count.max(1)])?;
    // Whether a requester's process still runs, asked again every few seconds at most.
    let mut known: std::collections::HashMap<i64, (bool, Instant)> = std::collections::HashMap::new();
    let mut alive = |pid: i64| -> bool {
        if pid as u32 == std::process::id() {
            return true;
        }
        match known.get(&pid) {
            Some((is, at)) if at.elapsed() < Duration::from_secs(5) => *is,
            _ => {
                let is = crate::crew::pid_alive(pid as u32);
                known.insert(pid, (is, Instant::now()));
                is
            }
        }
    };
    loop {
        if my_turn(db, id, slots, &mut alive)? {
            for k in 1..=slots.max(1) {
                let Some(slot) = try_slot(db, k)? else { continue };
                for n in 1..=rules.count.max(1) {
                    if let Some(lock) = try_lock(repo_root, pb, n)? {
                        return Ok((n, lock, slot));
                    }
                }
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
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
fn serve(repo_root: &Path, pb: &Playbook, rules: &LaneRules, lane: &Path, log: &Path, req: &Request, exec: &mut Exec) -> Result<Outcome> {
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
    let (exit_code, timed_out) = exec(lane, log, limit)?;
    let output = std::fs::read_to_string(log).map(|t| playbook::tail(&t)).unwrap_or_default();
    // Between requests, still holding the lane: best effort, it never fails the request.
    if let Some(record) = git_path(lane, SIZE_RECORD) {
        hygiene(lane, rules, &record, log);
    }
    Ok(Outcome { exit_code, timed_out, setup_ran, files_switched, output })
}

/// The file in a lane's git folder that keeps the size of its build directories after its last
/// request and after its first one: "<bytes> <first bytes>".
const SIZE_RECORD: &str = "nitely-lane-size";

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A lane's size cap in bytes: the playbook's, else twice its first build.
fn cap_bytes(rules: &LaneRules, first: u64) -> u64 {
    match rules.size_cap_gb {
        Some(gb) if gb > 0.0 => (gb * GB) as u64,
        _ => first.saturating_mul(2),
    }
}

/// What a lane's size record says: (size after the last request, size after the first), if any.
fn read_size(record: &Path) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(record).ok()?;
    let mut parts = text.split_whitespace().map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??))
}

/// Keeps a lane within its size cap (docs/build-lanes.md, "Lane hygiene"): over the cap, the
/// incremental caches in its build directories (`*/incremental`, cargo's) are emptied; over twice
/// the cap, its build directories go and the next request builds cold. Records the size after, and
/// the first build's size the first time. Nothing it removes is anyone's work. Says what it did in
/// the log.
fn hygiene(lane: &Path, rules: &LaneRules, record: &Path, log: &Path) {
    use std::io::Write;
    let measured = keep_size(lane, &rules.keep);
    let first = read_size(record).map(|(_, f)| f).filter(|f| *f > 0).unwrap_or(measured);
    let cap = cap_bytes(rules, first);
    let mut size = measured;
    let mut said = None;
    if cap > 0 && size > cap.saturating_mul(2) {
        for k in &rules.keep {
            std::fs::remove_dir_all(lane.join(k.trim_matches('/'))).ok();
        }
        size = keep_size(lane, &rules.keep);
        said = Some(format!("over twice its size cap ({} of {}): removed its build directories; the next request builds cold", human(measured), human(cap)));
    } else if cap > 0 && size > cap {
        for k in &rules.keep {
            let Ok(entries) = std::fs::read_dir(lane.join(k.trim_matches('/'))) else { continue };
            for e in entries.flatten() {
                std::fs::remove_dir_all(e.path().join("incremental")).ok();
            }
        }
        size = keep_size(lane, &rules.keep);
        said = Some(format!("over its size cap ({} of {}): emptied the incremental caches, now {}", human(measured), human(cap), human(size)));
    }
    std::fs::write(record, format!("{size} {first}")).ok();
    if let (Some(said), Ok(mut f)) = (said, std::fs::OpenOptions::new().append(true).open(log)) {
        writeln!(f, "nitely: lane {said}").ok();
    }
}

/// The bytes in a lane's build directories, symlinks not followed.
fn keep_size(lane: &Path, keep: &[String]) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    keep.iter().map(|k| walk(&lane.join(k.trim_matches('/')))).sum()
}

/// "8.3 GB", "640 MB".
fn human(bytes: u64) -> String {
    let gb = bytes as f64 / GB;
    if gb >= 1.0 { format!("{gb:.1} GB") } else { format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0)) }
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
        let command = playbook::expand(command, repo_root, lane, base).replace("{jobs}", &machine_budget().jobs.to_string());
        // Run again on failure as the gate's commands are (`[gate] retries`): a setup that fails
        // once, on a busy machine, then passes is not the change's fault.
        let mut tries = 0;
        loop {
            match run_logged(&command, lane, log, limit, machine_budget().jobs)? {
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
    // "<pid> <start time>\n<command>": the same command at the same number with the same start is
    // the process the request left; one that got the number since (running the same command in
    // another lane, say) has another start time.
    let (head, command) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    let mut head = head.split_whitespace();
    if let Some(Ok(pid)) = head.next().map(str::parse::<u32>) {
        let recorded = head.next();
        let same_command = !command.trim().is_empty() && crate::procs::command_line(pid).is_some_and(|line| line.contains(command.trim()));
        let same_start = recorded.is_some_and(|t| crate::procs::started(pid).as_deref() == Some(t));
        if same_command && same_start {
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
fn run_logged(command: &str, dir: &Path, log: &Path, limit: Duration, jobs: u32) -> Result<(Option<i32>, bool)> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "$ {command}")?;
    let mut cmd = playbook::shell_command(command);
    cmd.current_dir(dir).stdin(std::process::Stdio::null()).stdout(file.try_clone()?).stderr(file);
    // The machine's share for one request (NIT-199), for the build tools that read it.
    cmd.env("NITELY_JOBS", jobs.to_string()).env("CARGO_BUILD_JOBS", jobs.to_string()).env("MAKEFLAGS", format!("-j{jobs}"));
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
    // On Windows the command and all it starts go in a job that ends with this process: a crash of
    // the requester (even before the pid is written) leaves nothing running in the lane, and what
    // the command left running ends when it does (NIT-223).
    let guard = crate::procs::TreeGuard::new();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(p) = &pids {
                std::fs::remove_file(p).ok();
            }
            return Err(e).with_context(|| format!("starting {command}"));
        }
    };
    if let Some(g) = &guard {
        g.add(&child);
    }
    if let Some(p) = &pids {
        let started = crate::procs::started(child.id()).unwrap_or_default();
        std::fs::write(p, format!("{} {started}\n{command}", child.id())).ok();
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
    drop(guard);
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
    /// The size of its build directories after its last request, and the cap `hygiene` keeps it
    /// under, once it has run one.
    pub size_bytes: Option<u64>,
    pub cap_bytes: Option<u64>,
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
    /// The machine's budget, shared by every project's lanes (NIT-199).
    pub budget: Budget,
    /// Requests running on this machine now, across every project.
    pub running_on_machine: i64,
}

/// One repo's lanes for the app (NIT-205): `status`, plus the last finished run in each lane,
/// however long ago, so a lane whose last request ended in a lane problem shows it until it runs
/// again (`recent` is only the last few runs of the repo).
#[derive(Debug, Clone, Serialize)]
pub struct ShownLanes {
    #[serde(flatten)]
    pub lanes: Lanes,
    /// The last finished run of each lane that has one, in lane order.
    pub last: Vec<Run>,
}

/// The machine's build budget once the scheduler keeps one: "builds 3 of 4".
#[derive(Debug, Clone, Serialize)]
pub struct Slots {
    pub used: u32,
    pub total: u32,
}

/// Every connected repo's lanes, for the app's lanes strip and Settings.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub projects: Vec<ShownLanes>,
    /// Absent until the scheduler keeps a machine budget.
    pub slots: Option<Slots>,
}

/// The lanes of every connected repo whose playbook has `[lanes]`. A repo whose playbook does not
/// read is left out; its own views say why. It writes nothing (no lock files, no playbook copies),
/// so the read-only live preview can show it too: a lane is busy when a request runs in it.
pub fn overview(config: &crate::Config, db: &Db) -> Result<Overview> {
    let mut projects = Vec::new();
    for repo in &config.repos {
        let Some(project) = repo.project.as_deref() else { continue };
        let root = Path::new(&repo.root);
        let Ok(pb) = Playbook::load_with(root, config) else { continue };
        let Some(lanes) = status_with(db, root, &pb, project, false)? else { continue };
        let mut last = Vec::new();
        for l in &lanes.lanes {
            if let Some(r) = db.last_finished_lane_run(project, &repo.root, l.lane)? {
                last.push(r);
            }
        }
        projects.push(ShownLanes { lanes, last });
    }
    Ok(Overview { projects, slots: None })
}

impl Db {
    /// The newest finished run of lane `lane` of `repo`'s lanes in `project`, however long ago.
    pub fn last_finished_lane_run(&self, project: &str, repo: &str, lane: u32) -> Result<Option<Run>> {
        let mine = crate::paths::normalize(repo);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM lane_run WHERE project = ?1 COLLATE NOCASE AND lane = ?2 ORDER BY id DESC"
        ))?;
        let mut rows = stmt.query(params![project, lane as i64])?;
        while let Some(r) = rows.next()? {
            let (run, pid) = row(r)?;
            if crate::paths::normalize(&run.repo) != mine {
                continue;
            }
            // A run whose process died counts as finished (abandoned), even where that cannot be
            // written back (the read-only preview); one still going is not the lane's last.
            let run = settle(self, run, pid);
            if run.state != "queued" && run.state != "running" {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }
}

/// The repo's lanes, its queue and its last runs. None when its playbook has no `[lanes]`.
pub fn status(db: &Db, repo_root: &Path, pb: &Playbook, project: &str) -> Result<Option<Lanes>> {
    status_with(db, repo_root, pb, project, true)
}

/// `status`; with `probe_locks`, a lane is busy when its lock is held (which opens the lock file),
/// otherwise when a running request names it.
fn status_with(db: &Db, repo_root: &Path, pb: &Playbook, project: &str, probe_locks: bool) -> Result<Option<Lanes>> {
    let Some(rules) = pb.lanes.clone() else { return Ok(None) };
    let runs = db.lane_runs(project, 50)?;
    let root = repo_root.to_string_lossy();
    let mine = |r: &&Run| crate::paths::normalize(&r.repo) == crate::paths::normalize(&root);
    let mut queue: Vec<Run> = runs.iter().filter(mine).filter(|r| r.state == "queued" || r.state == "running").cloned().collect();
    queue.reverse();
    let mut lanes = Vec::new();
    for n in 1..=rules.count.max(1) {
        let path = lane_path(repo_root, pb, n);
        let made = path.join(".git").exists();
        let busy = if probe_locks { try_lock(repo_root, pb, n)?.is_none() } else { queue.iter().any(|r| r.state == "running" && r.lane == Some(n)) };
        let at = made.then(|| git(&path, &["rev-parse", "--short", "HEAD"]).ok().map(|s| s.trim().to_string())).flatten();
        let size = made.then(|| git_path(&path, SIZE_RECORD).and_then(|r| read_size(&r))).flatten();
        let (size_bytes, cap_bytes) = match size {
            Some((size, first)) => (Some(size), Some(cap_bytes(&rules, first))),
            None => (None, None),
        };
        lanes.push(LaneState { lane: n, path: path.to_string_lossy().into_owned(), made, busy, at, size_bytes, cap_bytes });
    }
    let recent = runs.iter().filter(mine).filter(|r| r.state != "queued" && r.state != "running").take(10).cloned().collect();
    // Every project's running requests, each read through `lane_run` so one whose process is gone
    // reads as abandoned and is not counted.
    let running_ids: Vec<i64> = db.conn.prepare("SELECT id FROM lane_run WHERE state = 'running'")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut running_on_machine = 0i64;
    for id in running_ids {
        if db.lane_run(id)?.is_some_and(|r| r.state == "running") {
            running_on_machine += 1;
        }
    }
    Ok(Some(Lanes {
        project: project.to_uppercase(),
        repo: root.into_owned(),
        dir: dir(repo_root, pb).to_string_lossy().into_owned(),
        rules,
        lanes,
        queue,
        recent,
        budget: machine_budget(),
        running_on_machine,
    }))
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
    fn the_overview_keeps_each_lanes_last_problem_however_many_runs_since() {
        let base = crate::paths::unique_temp("nitely-overview", "");
        let root = repo(&base);
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), "[lanes]
count = 2
").unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let insert = |lane: i64, state: &str, at: i64, error: Option<&str>| {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, lane, queued_at, error)
                     VALUES ('APP', ?1, 'claude', 'run', 's', 't', 'b', 'npm test', ?2, ?3, ?4, ?5)",
                    params![root_s, state, lane, at, error],
                )
                .unwrap();
        };
        insert(2, "lane_error", 1, Some("the switch failed twice"));
        for i in 0..600 {
            insert(1, "done", 10 + i, None);
        }
        let config = crate::Config {
            repos: vec![crate::config::RepoConfig { name: "app".into(), root: root_s.clone(), integration_branch: "main".into(), project: Some("APP".into()) }],
            ..Default::default()
        };
        let o = overview(&config, &db).unwrap();
        assert_eq!(o.projects.len(), 1);
        let shown = &o.projects[0];
        assert!(shown.lanes.recent.iter().all(|r| r.lane == Some(1)), "the last few runs are all lane 1's");
        assert_eq!(shown.last.iter().map(|r| (r.lane, r.state.as_str())).collect::<Vec<_>>(), [(Some(1), "done"), (Some(2), "lane_error")]);
        assert!(o.slots.is_none(), "no machine budget until the scheduler keeps one");
        assert!(!dir(&root, &playbook_with("[lanes]\n")).exists(), "reading the lanes writes nothing: no lane folder, no lock file");
        // A repo without [lanes] is left out.
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), "[gate]
commands = []
").unwrap();
        assert!(overview(&config, &db).unwrap().projects.is_empty());
        drop(db);
        std::fs::remove_dir_all(base).ok();
    }

    /// What a lane command leaves running ends with it on Windows (the job object), so a requester
    /// that crashes, or a command that starts a background process, leaves nothing in the lane.
    #[cfg(windows)]
    #[test]
    fn a_lane_command_takes_what_it_started_with_it() {
        let base = crate::paths::unique_temp("nitely-tree", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        // A marker no other process carries: an address only this test pings.
        let marker = "ping -n 40 127.0.0.97";
        let (code, timed_out) = run_logged(&format!("start \"\" /B {marker} >NUL & exit /b 0"), &base, &log, Duration::from_secs(30)).unwrap();
        assert_eq!((code, timed_out), (Some(0), false));
        let left = (0..25).all(|_| {
            std::thread::sleep(Duration::from_millis(200));
            crate::procs::holders(&base).is_empty() && !crate::procs::running_with(marker)
        });
        assert!(left, "the background ping ended with the command");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn hygiene_trims_a_lane_over_its_cap_and_builds_cold_over_twice_it() {
        let base = crate::paths::unique_temp("nitely-hygiene", "");
        let lane = base.join("lane");
        let record = base.join("size");
        let log = base.join("run.log");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&log, "").unwrap();
        let fill = |rel: &str, bytes: usize| {
            let p = lane.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![0u8; bytes]).unwrap();
        };
        let rules = LaneRules { keep: vec!["target".into()], ..Default::default() };
        // The first build sets the cap at twice its size: 2 000 bytes.
        fill("target/debug/app", 600);
        fill("target/debug/incremental/a", 400);
        fill("src/main.rs", 5_000);
        hygiene(&lane, &rules, &record, &log);
        assert_eq!(read_size(&record), Some((1_000, 1_000)), "only build directories count");

        // Over the cap: the incremental caches go, the rest stays.
        fill("target/debug/incremental/b", 1_500);
        hygiene(&lane, &rules, &record, &log);
        assert!(!lane.join("target/debug/incremental").exists() && lane.join("target/debug/app").exists());
        assert_eq!(read_size(&record), Some((600, 1_000)));

        // Over twice the cap: the build directories go; sources never.
        fill("target/release/big", 5_000);
        hygiene(&lane, &rules, &record, &log);
        assert!(!lane.join("target").exists() && lane.join("src/main.rs").exists());
        assert_eq!(read_size(&record), Some((0, 1_000)), "the first build's size is kept");
        let said = std::fs::read_to_string(&log).unwrap();
        assert!(said.contains("emptied the incremental caches") && said.contains("builds cold"), "{said}");

        // The playbook's cap wins over twice the first build.
        assert_eq!(cap_bytes(&LaneRules { size_cap_gb: Some(1.0), ..Default::default() }, 10), 1 << 30);
        std::fs::remove_dir_all(base).ok();
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
        assert!(Path::new(first.log.as_deref().unwrap()).starts_with(base.join("lanes")), "the log sits beside the database: {:?}", first.log);
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
        let started = crate::procs::started(child.id()).expect("the start time of a running child");
        // The same command at the same number but another start time is another process (a reused
        // pid in another lane): left alone.
        std::fs::write(pid_file(&lane).unwrap(), format!("{} 1\n{sleeper}", child.id())).unwrap();
        stop_leftovers(&lane);
        std::thread::sleep(Duration::from_millis(500));
        assert!(matches!(child.try_wait(), Ok(None)), "another start time: not the one the request left");
        std::fs::write(pid_file(&lane).unwrap(), format!("{} {started}\n{sleeper}", child.id())).unwrap();
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
    fn the_gate_runs_in_a_lane_once_per_tree_and_land_goes_first() {
        let base = crate::paths::unique_temp("nitely-lane-gate", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());
        // Proves where it ran: the lane gets the file, the worktree does not. The second command
        // fails the first time it ever runs (a mark outside the repo), then passes: a flake.
        let mark = base.join("flaked-once").to_string_lossy().replace('\\', "/");
        let (wrote, flaky) = if cfg!(windows) {
            ("echo ran > gate-ran.txt".to_string(), format!("if exist \"{mark}\" (exit 0) else (type nul > \"{mark}\" & exit 1)"))
        } else {
            ("echo ran > gate-ran.txt".to_string(), format!("if [ -e '{mark}' ]; then exit 0; else touch '{mark}'; exit 1; fi"))
        };
        let text = format!("[gate]\ncommands = [{wrote:?}, {flaky:?}]\n[lanes]\n");
        std::fs::create_dir_all(root.join(".nitely")).unwrap();
        std::fs::write(root.join(playbook::PLAYBOOK_PATH), &text).unwrap();
        let g = |args: &[&str]| assert!(playbook::run_git(Command::new("git").arg("-C").arg(&root).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "playbook"]);
        let pb = Playbook::load(&root).unwrap();
        let rows = |db: &Db| db.lane_runs("", 50).unwrap();

        let gate = playbook::run_gate(&db, &root, &root, Some("APP-1"), &actor).unwrap();
        assert!(gate.passed, "{gate:?}");
        assert_eq!(gate.flaked.len(), 1, "the second command passed on its second run");
        assert!(!root.join("gate-ran.txt").exists(), "nothing ran in the worktree");
        assert!(lane_path(&root, &pb, 1).join("gate-ran.txt").exists(), "it ran in the lane");
        let first = rows(&db);
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].kind.as_str(), first[0].command.lines().count()), ("gate", 2));
        let kinds: Vec<String> = db.timeline("APP-1").unwrap().into_iter().map(|t| t.kind).collect();
        assert!(kinds.ends_with(&["lane_run".to_string(), "gate_flaked".to_string(), "gate_passed".to_string()]), "{kinds:?}");

        // The same tree again: answered from that run, nothing runs.
        let again = playbook::run_gate(&db, &root, &root, Some("APP-1"), &actor).unwrap();
        assert!(again.passed && again.results.iter().all(|r| r.skipped && r.output.contains("not run again")), "{again:?}");
        assert_eq!(rows(&db).len(), 1, "no new run");

        // Uncommitted work is not what a gate builds: it says so, and the record answers it.
        std::fs::write(root.join("src/a.txt"), "changed\n").unwrap();
        let dirty = playbook::run_gate(&db, &root, &root, Some("APP-1"), &actor).unwrap();
        assert!(dirty.passed && dirty.warnings.iter().any(|w| w.contains("uncommitted work")), "{dirty:?}");
        assert_eq!(dirty.commit, gate.commit, "it reports the branch head, the commit the review sees");
        assert_eq!(rows(&db).len(), 1);

        // Another tree, as land asks: a land request, first in line.
        g(&["commit", "-q", "-am", "change"]);
        let landing = playbook::run_gate_as(&db, &root, &root, Some("APP-1"), &actor, Kind::Land).unwrap();
        assert_eq!(landing.lane_run, Some(rows(&db)[0].id));
        assert!(landing.passed, "{landing:?}");
        let now = rows(&db);
        assert_eq!(now.len(), 2);
        assert_eq!(now[0].kind, "land");
        let priority: i64 = db.conn.query_row("SELECT priority FROM lane_run WHERE id = ?1", [now[0].id], |r| r.get(0)).unwrap();
        assert_eq!(priority, 1);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_budget_comes_from_cores_and_memory_unless_set() {
        let b = budget_for(32, Some(64), None);
        assert_eq!((b.slots, b.jobs, b.from.as_str()), (4, 8, "machine"));
        assert_eq!(budget_for(32, Some(16), None).slots, 2, "memory caps it");
        assert_eq!((budget_for(4, Some(8), None).slots, budget_for(4, Some(8), None).jobs), (1, 4), "always one");
        assert_eq!(budget_for(8, None, None).slots, 1, "memory unknown: cores decide");
        let set = budget_for(32, Some(64), Some((6, "settings")));
        assert_eq!((set.slots, set.jobs, set.from.as_str()), (6, 5, "settings"));
        assert_eq!(budget_for(32, Some(64), Some((0, "settings"))).from, "machine", "0 is not a setting");
    }

    #[test]
    fn the_queue_goes_by_priority_then_one_per_project_then_age() {
        let w = |id, project: &str, priority| Waiting { id, project: project.into(), repo: format!("C:/Code/{project}"), priority };
        let none = std::collections::HashMap::new();
        let order: Vec<i64> = in_order(vec![w(1, "A", 3), w(2, "A", 3), w(3, "B", 3), w(4, "A", 2), w(5, "C", 1), w(6, "B", 3)], &none).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [5, 4, 1, 3, 2, 6]);
        // A1 started: A2 waits behind B1, though it is older.
        // A was served last (A1 started): A2 waits behind B1, though it is older.
        let a_served = std::collections::HashMap::from([("A".to_string(), 10)]);
        let order: Vec<i64> = in_order(vec![w(2, "A", 3), w(3, "B", 3)], &a_served).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [3, 2]);
        // Both served, B more recently: A's goes first.
        let both = std::collections::HashMap::from([("A".to_string(), 10), ("B".to_string(), 20)]);
        let order: Vec<i64> = in_order(vec![w(2, "A", 3), w(3, "B", 3)], &both).into_iter().map(|w| w.id).collect();
        assert_eq!(order, [2, 3]);
    }

    #[test]
    fn a_request_waits_for_its_repo_and_a_slot_but_not_for_a_busy_repo() {
        let base = crate::paths::unique_temp("nitely-lane-turn", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let me = std::process::id() as i64;
        let row = |project: &str, state: &str, priority: i64, lanes: i64, pid: i64| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority, lane_count, started_at)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', ?3, 'local', 1, ?4, ?5, ?6, CASE WHEN ?3 = 'queued' THEN NULL ELSE 2 END)",
                    params![project, format!("C:/Code/{project}"), state, pid, priority, lanes],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        let mut alive = |pid: i64| pid == me;
        // B's one lane is busy; A's request behind B's waiting one still goes, with a slot free.
        row("B", "running", 3, 1, me);
        let b_waits = row("B", "queued", 3, 1, me);
        let a = row("A", "queued", 3, 1, me);
        assert!(!my_turn(&db, b_waits, 2, &mut alive).unwrap(), "B has one running: A's first goes before B's second");
        assert!(my_turn(&db, b_waits, 3, &mut alive).unwrap(), "with a slot for each, B may try; its lane's lock decides");
        assert!(my_turn(&db, a, 2, &mut alive).unwrap(), "a busy repo does not hold A up");
        assert!(!my_turn(&db, a, 1, &mut alive).unwrap(), "no free slot: the one is B's running request");
        // A second request of A waits behind the first.
        let a2 = row("A", "queued", 3, 1, me);
        assert!(!my_turn(&db, a2, 4, &mut alive).unwrap());
        // A land request jumps the line; a dead requester's row holds nothing.
        let land = row("C", "queued", 1, 1, me);
        assert!(my_turn(&db, land, 2, &mut alive).unwrap());
        assert!(!my_turn(&db, a, 2, &mut alive).unwrap(), "the land request takes the one free slot first");
        row("D", "running", 3, 1, 999_999);
        assert!(my_turn(&db, a, 3, &mut alive).unwrap(), "a dead requester's running row holds no slot");
        let mut all_alive = |_: i64| true;
        assert!(!my_turn(&db, a, 3, &mut all_alive).unwrap(), "while it lives, it does");
        std::fs::remove_dir_all(base).ok();

        // A1 has run and finished; A2 and B1 wait for one slot: B1 goes first.
        let base = crate::paths::unique_temp("nitely-lane-turn2", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let add = |project: &str, state: &str, queued: i64, started: Option<i64>| -> i64 {
            db.conn
                .execute(
                    "INSERT INTO lane_run (project, repo, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, started_at, pid, priority, lane_count)
                     VALUES (?1, ?2, 'x', 'run', 's', 't', 'b', 'c', ?3, 'local', ?4, ?5, ?6, 3, 1)",
                    params![project, format!("C:/Code/{project}"), state, queued, started, me],
                )
                .unwrap();
            db.conn.last_insert_rowid()
        };
        // A1 started (10) before A2 (11) and B1 (12) were even queued, and has finished.
        add("A", "done", 9, Some(10));
        let a2 = add("A", "queued", 11, None);
        let b1 = add("B", "queued", 12, None);
        let mut alive = |pid: i64| pid == me;
        assert!(my_turn(&db, b1, 1, &mut alive).unwrap(), "B was never served; A was, last");
        assert!(!my_turn(&db, a2, 1, &mut alive).unwrap());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_command_gets_the_machines_share_of_cores() {
        let base = crate::paths::unique_temp("nitely-lane-jobs", "");
        let root = repo(&base);
        let db = Db::open(&base.join("t.db")).unwrap();
        let pb = playbook_with("[lanes]\n");
        let snap = snapshot(&root).unwrap();
        let echo = if cfg!(windows) { "echo jobs={jobs} env=%NITELY_JOBS% cargo=%CARGO_BUILD_JOBS%" } else { "echo jobs={jobs} env=$NITELY_JOBS cargo=$CARGO_BUILD_JOBS" };
        let req = Request { project: "APP", issue: None, kind: Kind::Run, snapshot: &snap, command: echo };
        let ran = run(&db, &root, &pb, &req, &Actor::Agent("claude:test".into()), &mut |_| {}).unwrap();
        let n = machine_budget().jobs;
        assert!(ran.output.contains(&format!("jobs={n} env={n} cargo={n}")), "{}", ran.output);
        let lanes = status(&db, &root, &pb, "APP").unwrap().unwrap();
        assert_eq!(lanes.budget.jobs, n);
        assert_eq!(lanes.running_on_machine, 0);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_command_past_its_time_is_stopped() {
        let base = crate::paths::unique_temp("nitely-lane-time", "");
        std::fs::create_dir_all(&base).unwrap();
        let log = base.join("run.log");
        let sleeper = if cfg!(windows) { "ping -n 30 127.0.0.1 >NUL" } else { "sleep 30" };
        let started = Instant::now();
        let (code, timed_out) = run_logged(sleeper, &base, &log, Duration::from_secs(1), 1).unwrap();
        assert!(timed_out && code.is_none());
        assert!(started.elapsed() < Duration::from_secs(20), "stopped, not waited out");
        std::fs::remove_dir_all(base).ok();
    }
}
