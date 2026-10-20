//! Explicit machine and CI executors. Permissions and transport argv are machine-local, never
//! supplied by a request or inherited from a backlog relay token.
use crate::{
    db::now,
    lanes::{self, Kind, Request, Run, Snapshot},
    playbook::{self, Playbook},
    Actor, Config, Db,
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_REQUEST: u64 = 12 * 1024 * 1024;
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequiredCheck {
    pub name: String,
    pub app_id: u64,
}

/// Written only by the person configuring an executor. Merely naming a machine in a playbook
/// grants no permission to launch a transport, publish source, or execute received commands.
#[derive(Default, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub machines: BTreeMap<String, Machine>,
    #[serde(default)]
    pub receive_projects: Vec<String>,
    #[serde(default)]
    pub ci_projects: Vec<String>,
}
#[derive(Deserialize)]
pub struct Machine {
    pub command: Vec<String>,
    pub project: String,
    pub projects: Vec<String>,
}
impl Settings {
    pub fn load() -> Result<Self> {
        let path = crate::paths::data_dir().join("executors.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
}
fn allowed(projects: &[String], project: &str) -> bool {
    !project.is_empty() && projects.iter().any(|p| p.eq_ignore_ascii_case(project))
}
fn oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}
fn safe_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
fn selection(pb: &Playbook) -> &str {
    pb.lanes
        .as_ref()
        .map(|r| r.executor.as_str())
        .unwrap_or("local")
}

#[derive(Serialize, Deserialize)]
pub struct WireRequest {
    pub version: u32,
    pub id: String,
    pub project: String,
    pub base: String,
    pub tree: String,
    pub patch: String,
    pub kind: Kind,
    pub steps: Vec<(String, Option<String>)>,
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub id: String,
    pub tree: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub results: Vec<playbook::CommandResult>,
    pub flaked: Vec<playbook::Flake>,
    pub remote_run: Option<i64>,
    /// Only these pre-execution failures permit an explicitly reported local fallback.
    pub unavailable: Option<String>,
}
impl WireRequest {
    fn validate(&self) -> Result<()> {
        if self.version != 1 || !safe_id(&self.id) || !oid(&self.base) || !oid(&self.tree) {
            bail!("invalid execution protocol identity");
        }
        if self.steps.is_empty()
            || self.steps.len() > 64
            || self.steps.iter().any(|(c, s)| {
                c.is_empty() || c.len() > 65536 || s.as_ref().is_some_and(|s| s.len() > 4096)
            })
        {
            bail!("execution request needs 1–64 bounded commands");
        }
        if self.project.is_empty()
            || self.project.len() > 64
            || !self
                .project
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("invalid execution project");
        }
        if self.patch.len() as u64 > MAX_REQUEST {
            bail!("snapshot patch exceeds transport limit");
        }
        Ok(())
    }
    fn unavailable(&self, why: &str) -> Reply {
        Reply {
            version: 1,
            id: self.id.clone(),
            tree: self.tree.clone(),
            passed: false,
            exit_code: None,
            timed_out: false,
            results: vec![],
            flaked: vec![],
            remote_run: None,
            unavailable: Some(why.into()),
        }
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self> {
        let p = crate::paths::unique_temp("nitely-executor", "");
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Bounded shell-free transport, including bounded stdout/stderr. Input is a file, so a peer
/// that never reads stdin cannot block our deadline. Never retries a request after dispatch.
fn process(
    mut cmd: std::process::Command,
    input: &[u8],
    timeout: Duration,
    limit: u64,
) -> Result<Vec<u8>> {
    let dir = Scratch::new()?;
    let stdin = dir.0.join("in");
    let stdout = dir.0.join("out");
    let stderr = dir.0.join("err");
    std::fs::write(&stdin, input)?;
    cmd.stdin(std::fs::File::open(stdin)?)
        .stdout(std::fs::File::create(&stdout)?)
        .stderr(std::fs::File::create(&stderr)?);
    let guard = crate::procs::TreeGuard::with(crate::procs::Contain {
        memory_cap: None,
        low_priority: true,
        end_with_guard: true,
    })
    .context("cannot contain executor transport")?;
    let mut child = guard.spawn(&mut cmd)?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout
            || std::fs::metadata(&stdout)?.len() > limit
            || std::fs::metadata(&stderr)?.len() > 65536
        {
            guard.stop();
            let _ = child.kill();
            let _ = child.wait();
            bail!("executor transport exceeded its time/output bound; remote outcome may be unknown, so it was not retried");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    guard.end(Duration::from_secs(2));
    if std::fs::metadata(&stdout)?.len() > limit || std::fs::metadata(&stderr)?.len() > 65536 {
        bail!("executor response exceeds output bound");
    }
    if !status.success() {
        let mut error = String::new();
        std::fs::File::open(stderr)?
            .take(4096)
            .read_to_string(&mut error)
            .ok();
        bail!("executor command failed ({status}): {error}");
    }
    Ok(std::fs::read(stdout)?)
}
fn git_bytes(root: &Path, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<Vec<u8>> {
    let mut cmd = playbook::shell_free_command("git");
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.env("GIT_AUTHOR_NAME", "Nitely")
        .env("GIT_AUTHOR_EMAIL", "nitely@localhost")
        .env("GIT_COMMITTER_NAME", "Nitely")
        .env("GIT_COMMITTER_EMAIL", "nitely@localhost");
    process(cmd, input, Duration::from_secs(60), MAX_REQUEST)
}

fn git(root: &Path, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<String> {
    Ok(String::from_utf8(git_bytes(root, args, input, index)?)?
        .trim()
        .into())
}

pub fn run(
    db: &Db,
    root: &Path,
    pb: &Playbook,
    req: &Request,
    actor: &Actor,
    on_event: &mut dyn FnMut(&Run),
) -> Result<Option<Run>> {
    if selection(pb) == "local" || selection(pb) == "ci" {
        return Ok(None);
    } // CI is gate-only.
    let steps = vec![(req.command.into(), None)];
    execute(db, root, pb, req, &steps, actor, on_event).map(|x| x.map(|(r, _)| r))
}

#[allow(clippy::too_many_arguments)]
pub fn gate(
    db: &Db,
    root: &Path,
    pb: &Playbook,
    snap: &Snapshot,
    issue: Option<&str>,
    kind: Kind,
    steps: &[(String, Option<String>)],
    actor: &Actor,
    warnings: &[String],
) -> Result<Option<lanes::GateRun>> {
    if selection(pb) == "local" {
        return Ok(None);
    }
    let config = Config::load_read_only()?;
    let project = config
        .repo_for_main(&root.to_string_lossy())
        .and_then(|r| r.project.clone())
        .unwrap_or_default();
    let command = steps
        .iter()
        .filter(|(_, skip)| skip.is_none())
        .map(|(c, _)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let req = Request {
        project: &project,
        issue,
        kind,
        snapshot: snap,
        command: &command,
    };
    let Some((run, reply)) = execute(db, root, pb, &req, steps, actor, &mut |_| {})? else {
        return Ok(None);
    };
    Ok(Some(lanes::GateRun {
        yolo_skipped: false,
        commit: snap.commit[..7].into(),
        results: reply.results,
        passed: reply.passed,
        flaked: reply.flaked,
        run: Some(run.id),
        warnings: warnings.to_vec(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn execute(
    db: &Db,
    root: &Path,
    pb: &Playbook,
    req: &Request,
    steps: &[(String, Option<String>)],
    actor: &Actor,
    on_event: &mut dyn FnMut(&Run),
) -> Result<Option<(Run, Reply)>> {
    let settings = Settings::load()?;
    let executor = selection(pb);
    let machine = if executor == "ci" {
        if !allowed(&settings.ci_projects, req.project) {
            bail!("CI source publication is not enabled for this project in executors.json");
        }
        None
    } else {
        let name = executor
            .strip_prefix("machine:")
            .context("executor must be local, ci, or machine:<name>")?;
        let m = settings
            .machines
            .get(name)
            .context("machine executor is not configured in executors.json")?;
        if !allowed(&m.projects, req.project) || m.command.is_empty() {
            bail!("machine executor does not permit this project");
        }
        Some(m)
    };
    let kind = match req.kind {
        Kind::Run => "run",
        Kind::Gate => "gate",
        Kind::Land => "land",
    };
    db.conn.execute("INSERT INTO lane_run (project,repo,issue_key,requested_by,kind,snapshot,tree,base,command,state,executor,queued_at,started_at,pid,pid_started,scheduler)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'running',?10,?11,?11,?12,?13,3)",
        params![req.project,root.to_string_lossy(),req.issue,actor.id(),kind,req.snapshot.commit,req.snapshot.tree,req.snapshot.base,req.command,executor,now(),std::process::id(),crate::procs::me().1])?;
    let id = db.conn.last_insert_rowid();
    on_event(&db.lane_run(id)?.context("executor run missing")?);
    let nonce = crate::paths::unique_temp("request", "")
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    db.conn.execute(
        "UPDATE lane_run SET output=?2 WHERE id=?1",
        params![id, format!("executor request {nonce}; outcome pending")],
    )?;
    let outcome = (|| -> Result<Reply> {
        if machine.is_none() {
            return ci(root, pb, req, &nonce);
        }
        let machine = machine.unwrap();
        // The integration commit is normally already on the remote. Never ship full history.
        let base = git(
            root,
            &["merge-base", &req.snapshot.commit, pb.integration_branch()],
            &[],
            None,
        )?;
        let patch = git_bytes(
            root,
            &[
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                &base,
                &req.snapshot.commit,
                "--",
            ],
            &[],
            None,
        )?;
        let wire = WireRequest {
            version: 1,
            id: nonce.clone(),
            project: machine.project.clone(),
            base,
            tree: req.snapshot.tree.clone(),
            patch: base64::engine::general_purpose::STANDARD.encode(&patch),
            kind: req.kind,
            steps: steps.to_vec(),
        };
        wire.validate()?;
        let bytes = serde_json::to_vec(&wire)?;
        if bytes.len() as u64 > MAX_REQUEST {
            bail!("snapshot exceeds the 12 MiB executor request bound");
        }
        let mut cmd = playbook::shell_free_command(&machine.command[0]);
        cmd.args(&machine.command[1..]);
        let timeout = Duration::from_secs(
            pb.lanes
                .as_ref()
                .map(|r| r.timeout_minutes.clamp(1, 120))
                .unwrap_or(30)
                * 60,
        );
        let reply: Reply = serde_json::from_slice(&process(cmd, &bytes, timeout, MAX_OUTPUT)?)?;
        validate_reply(&wire, &reply)?;
        Ok(reply)
    })();
    let result = match outcome {
        Ok(reply) => {
            let fallback = reply.unavailable.is_some();
            let output = serde_json::to_string(&reply)?;
            db.conn.execute("UPDATE lane_run SET state=?2,ended_at=?3,exit_code=?4,output=?5,error=?6,timed_out=?7 WHERE id=?1",
                params![id,if fallback {"lane_error"} else {"done"},now(),reply.exit_code,playbook::tail(&output),reply.unavailable,reply.timed_out])?;
            if let Some(why) = &reply.unavailable {
                if let Some(key) = req.issue {
                    db.log_event(
                        actor,
                        Some(key),
                        None,
                        "executor_fallback",
                        &format!("{executor} run {id}: {why}; falling back to a local lane"),
                    )?;
                }
                None
            } else {
                Some((db.lane_run(id)?.context("executor run missing")?, reply))
            }
        }
        Err(error) => {
            db.conn.execute(
                "UPDATE lane_run SET state='lane_error',ended_at=?2,error=?3 WHERE id=?1",
                params![id, now(), format!("{error:#}")],
            )?;
            return Err(error);
        }
    };
    let row = db.lane_run(id)?.context("executor run missing")?;
    on_event(&row);
    if let Some(key) = req.issue {
        db.log_event(
            actor,
            Some(key),
            None,
            "lane_run",
            &format!("{executor} run {id}: {}", row.state),
        )?;
    }
    Ok(result)
}

fn validate_reply(request: &WireRequest, reply: &Reply) -> Result<()> {
    if reply.version != 1 || reply.id != request.id || reply.tree != request.tree {
        bail!("executor reply does not cover this request and exact tree");
    }
    if reply.passed && (reply.exit_code != Some(0) || reply.timed_out) {
        bail!("executor pass has no successful exit");
    }
    if let Some(reason) = &reply.unavailable {
        if reply.passed
            || reply.remote_run.is_some()
            || reply.exit_code.is_some()
            || reply.timed_out
            || !reply.results.is_empty()
            || !matches!(reason.as_str(), "base_unavailable" | "patch_rejected")
        {
            bail!("invalid executor fallback response");
        }
    } else if reply.passed
        && (reply.results.len() != request.steps.len()
            || reply
                .results
                .iter()
                .zip(&request.steps)
                .any(|(r, (c, skip))| !r.ok || &r.command != c || r.skipped != skip.is_some()))
    {
        bail!("executor pass does not cover every requested command");
    }
    Ok(())
}

/// Receiver permission is checked before reading the request's project, git objects or commands.
/// A FULL-synchronous SQLite receipt reserves the ID before execution. A duplicate never executes again, including
/// after a crash; an incomplete receipt requires explicit diagnosis, not automatic replay.
pub fn receive(
    config: &Config,
    db: &Db,
    settings: &Settings,
    project: &str,
    bytes: &[u8],
) -> Result<Reply> {
    if !allowed(&settings.receive_projects, project) {
        bail!("execution receiving is not enabled for this project");
    }
    if bytes.len() as u64 > MAX_REQUEST {
        bail!("executor request exceeds 12 MiB");
    }
    let request: WireRequest = serde_json::from_slice(bytes)?;
    request.validate()?;
    if !request.project.eq_ignore_ascii_case(project) {
        bail!("execution request is for another project");
    }
    let repos: Vec<_> = config
        .repos
        .iter()
        .filter(|r| {
            r.project
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(project))
        })
        .collect();
    if repos.len() != 1 {
        bail!("execution receiving requires exactly one connected repo for the project");
    }
    let root = Path::new(&repos[0].root);
    let folder = lanes::state_dir(db).join("executor-receipts");
    std::fs::create_dir_all(&folder)?;
    let mut ledger = rusqlite::Connection::open(folder.join("receipts.sqlite"))?;
    ledger.busy_timeout(Duration::from_secs(5))?;
    ledger.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS receipt (id TEXT PRIMARY KEY, project TEXT NOT NULL, digest TEXT NOT NULL, result TEXT);")?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    let tx = ledger.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let prior: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT digest,result FROM receipt WHERE id=?1",
            [&request.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((previous, result)) = prior {
        if previous != digest {
            bail!("executor request ID was already used for different content");
        }
        return serde_json::from_str(&result.context(
            "execution is still running or its outcome is unknown; this ID will not be replayed",
        )?)
        .map_err(Into::into);
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM receipt", [], |r| r.get(0))?;
    let size: i64 = tx.query_row(
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
        [],
        |r| r.get(0),
    )?;
    if count >= 5000 || size >= 128 * 1024 * 1024 {
        bail!("executor receipt capacity reached; archive the completed ledger before accepting more requests");
    }
    tx.execute(
        "INSERT INTO receipt(id,project,digest) VALUES (?1,?2,?3)",
        params![request.id, project, digest],
    )?;
    tx.commit()?;
    let reply = receive_new(config, db, root, &request)?;
    ledger.execute(
        "UPDATE receipt SET result=?2 WHERE id=?1",
        params![request.id, serde_json::to_string(&reply)?],
    )?;
    Ok(reply)
}

pub fn result(db: &Db, settings: &Settings, project: &str, id: &str) -> Result<Reply> {
    if !allowed(&settings.receive_projects, project) || !safe_id(id) {
        bail!("execution result access is not enabled");
    }
    let ledger = rusqlite::Connection::open_with_flags(
        lanes::state_dir(db).join("executor-receipts/receipts.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let result: Option<String> = ledger.query_row(
        "SELECT result FROM receipt WHERE id=?1 AND project=?2 COLLATE NOCASE",
        params![id, project],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&result.context(
        "execution has no completed receipt; do not replay it with a new ID",
    )?)?)
}

fn receive_new(config: &Config, db: &Db, root: &Path, request: &WireRequest) -> Result<Reply> {
    if git(
        root,
        &["cat-file", "-e", &format!("{}^{{commit}}", request.base)],
        &[],
        None,
    )
    .is_err()
        && git(
            root,
            &["fetch", "--no-tags", "origin", &request.base],
            &[],
            None,
        )
        .is_err()
    {
        return Ok(request.unavailable("base_unavailable"));
    }
    let scratch = Scratch::new()?;
    let index = scratch.0.join("index");
    let patch = base64::engine::general_purpose::STANDARD.decode(&request.patch)?;
    git(root, &["read-tree", &request.base], &[], Some(&index))?;
    if !patch.is_empty()
        && git(
            root,
            &["apply", "--cached", "--binary", "--whitespace=nowarn", "-"],
            &patch,
            Some(&index),
        )
        .is_err()
    {
        return Ok(request.unavailable("patch_rejected"));
    }
    let tree = git(root, &["write-tree"], &[], Some(&index))?;
    if tree != request.tree {
        bail!("received snapshot tree does not match the requested tree");
    }
    let commit = git(
        root,
        &[
            "commit-tree",
            &tree,
            "-p",
            &request.base,
            "-m",
            "Nitely executor snapshot",
        ],
        &[],
        None,
    )?;
    let snap = Snapshot {
        commit,
        tree,
        base: request.base.clone(),
        made: true,
    };
    let mut pb = Playbook::load_with(root, config)?;
    let rules = pb
        .lanes
        .as_mut()
        .context("receiving project has no build lanes")?;
    rules.executor = "local".into(); // Never forward again.
    rules.timeout_minutes = rules.timeout_minutes.clamp(1, 120);
    let actor = Actor::Agent("executor".into());
    let mut reply = Reply {
        version: 1,
        id: request.id.clone(),
        tree: request.tree.clone(),
        passed: true,
        exit_code: Some(0),
        timed_out: false,
        results: vec![],
        flaked: vec![],
        remote_run: None,
        unavailable: None,
    };
    for (command, skip) in &request.steps {
        if let Some(why) = skip {
            reply.results.push(playbook::CommandResult {
                command: command.clone(),
                ok: true,
                seconds: 0,
                output: format!("skipped: {why}"),
                skipped: true,
            });
            continue;
        }
        let req = Request {
            project: &request.project,
            issue: None,
            kind: request.kind,
            snapshot: &snap,
            command,
        };
        let mut failures = Vec::new();
        let row = loop {
            let row = lanes::run(db, root, &pb, &req, &actor, &mut |_| {})?;
            if row.passed()
                || row.timed_out
                || row.error.is_some()
                || failures.len() as u32
                    >= if request.kind == Kind::Run {
                        0
                    } else {
                        pb.gate.retries.min(3)
                    }
            {
                break row;
            }
            failures.push(row.output);
        };
        if row.passed() && !failures.is_empty() {
            reply.flaked.push(playbook::Flake {
                command: command.clone(),
                tests: failures
                    .iter()
                    .flat_map(|s| playbook::failed_tests(s))
                    .collect(),
                output: failures.remove(0),
            });
        }
        reply.remote_run = Some(row.id);
        reply.exit_code = row.exit_code;
        reply.timed_out = row.timed_out;
        reply.results.push(playbook::CommandResult {
            command: command.clone(),
            ok: row.passed(),
            seconds: row
                .ended_at
                .unwrap_or(now())
                .saturating_sub(row.started_at.unwrap_or(now()))
                .max(0) as u64,
            output: row.error.clone().unwrap_or_else(|| row.output.clone()),
            skipped: false,
        });
        if !row.passed() {
            reply.passed = false;
            break;
        }
    }
    Ok(reply)
}

fn ci(root: &Path, pb: &Playbook, req: &Request, nonce: &str) -> Result<Reply> {
    let checks = &pb
        .lanes
        .as_ref()
        .context("CI needs lane settings")?
        .ci_checks;
    if checks.is_empty() || checks.iter().any(|c| c.name.is_empty() || c.app_id == 0) {
        bail!("CI requires explicit check names and app IDs in lanes.ci_checks");
    }
    let slug = crate::git::github_slug(&root.to_string_lossy())
        .context("CI executor currently requires a GitHub origin")?;
    let reference = format!("refs/heads/nitely/run/{nonce}");
    let since = chrono::Utc::now();
    if let Err(error) = git(
        root,
        &[
            "push",
            &format!("--force-with-lease={reference}:"),
            "origin",
            &format!("{}:{reference}", req.snapshot.commit),
        ],
        &[],
        None,
    ) {
        let cleanup = git(
            root,
            &[
                "push",
                &format!("--force-with-lease={reference}:{}", req.snapshot.commit),
                "origin",
                &format!(":{reference}"),
            ],
            &[],
            None,
        );
        bail!(
            "CI dispatch failed: {error}; transient ref {reference} cleanup: {}",
            if cleanup.is_ok() {
                "done"
            } else {
                "unconfirmed; inspect this exact ref"
            }
        );
    }
    let outcome = (|| -> Result<Reply> {
        let client = crate::github::Client::new();
        let started = Instant::now();
        let timeout =
            Duration::from_secs(pb.lanes.as_ref().unwrap().timeout_minutes.clamp(1, 120) * 60);
        loop {
            let mut runs = client.get(&format!(
                "https://api.github.com/repos/{slug}/commits/{}/check-runs?filter=all&per_page=100",
                req.snapshot.commit
            ))?;
            let suites = client.get(&format!(
                "https://api.github.com/repos/{slug}/commits/{}/check-suites?per_page=100",
                req.snapshot.commit
            ))?;
            if suites["total_count"].as_u64().unwrap_or(0) > 100 {
                bail!("CI has more than 100 check suites; refusing incomplete evidence");
            }
            let branch = reference.strip_prefix("refs/heads/").unwrap();
            let ids: Vec<_> = suites["check_suites"]
                .as_array()
                .context("CI returned no check suites")?
                .iter()
                .filter(|s| {
                    s["head_branch"].as_str() == Some(branch)
                        && s["head_sha"].as_str() == Some(req.snapshot.commit.as_str())
                })
                .filter_map(|s| s["id"].as_u64())
                .collect();
            if let Some(items) = runs["check_runs"].as_array_mut() {
                items.retain(|r| {
                    r["check_suite"]["id"]
                        .as_u64()
                        .is_some_and(|id| ids.contains(&id))
                });
            }
            if runs["total_count"].as_u64().unwrap_or(0) > 100 {
                bail!("CI has more than 100 check runs; refusing incomplete check evidence");
            }
            if let Some(passed) = checks_result(&runs, checks, &req.snapshot.commit, since) {
                let url = format!(
                    "https://github.com/{slug}/commit/{}/checks",
                    req.snapshot.commit
                );
                return Ok(Reply {
                    version: 1,
                    id: nonce.into(),
                    tree: req.snapshot.tree.clone(),
                    passed,
                    exit_code: Some(if passed { 0 } else { 1 }),
                    timed_out: false,
                    results: vec![playbook::CommandResult {
                        command: "CI required checks".into(),
                        ok: passed,
                        seconds: started.elapsed().as_secs(),
                        output: url,
                        skipped: false,
                    }],
                    flaked: vec![],
                    remote_run: None,
                    unavailable: None,
                });
            }
            if started.elapsed() >= timeout {
                bail!(
                    "CI checks did not finish before timeout; missing checks never count as a pass"
                );
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    })();
    let cleanup = git(
        root,
        &[
            "push",
            &format!("--force-with-lease={reference}:{}", req.snapshot.commit),
            "origin",
            &format!(":{reference}"),
        ],
        &[],
        None,
    );
    match (outcome, cleanup) {
        (Ok(reply), Ok(_)) => Ok(reply),
        (Err(e), Ok(_)) => Err(e),
        (result, Err(e)) => bail!(
            "CI ref cleanup failed for {reference}: {e}; execution outcome: {}",
            result
                .as_ref()
                .map(|r| if r.passed { "passed" } else { "failed" })
                .unwrap_or("unknown")
        ),
    }
}

fn checks_result(
    runs: &serde_json::Value,
    required: &[RequiredCheck],
    sha: &str,
    since: chrono::DateTime<chrono::Utc>,
) -> Option<bool> {
    let runs = runs["check_runs"].as_array()?;
    let mut waiting = false;
    for check in required {
        let matching: Vec<_> = runs
            .iter()
            .filter(|r| {
                r["name"].as_str() == Some(&check.name)
                    && r["app"]["id"].as_u64() == Some(check.app_id)
                    && r["head_sha"].as_str() == Some(sha)
                    && r["started_at"]
                        .as_str()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .is_some_and(|t| t.timestamp() >= since.timestamp())
            })
            .collect();
        let Some(run) = matching
            .into_iter()
            .max_by_key(|r| r["id"].as_u64().unwrap_or(0))
        else {
            waiting = true;
            continue;
        };
        if run["status"] != "completed" {
            waiting = true;
            continue;
        }
        if run["conclusion"] != "success" {
            return Some(false);
        }
    }
    if waiting {
        None
    } else {
        Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_bounds_a_peer_that_never_reads_or_never_stops_writing() {
        let mut cmd = playbook::shell_free_command("node");
        cmd.args(["-e", "setInterval(()=>{},1000)"]);
        assert!(process(cmd, &[0; 8192], Duration::from_millis(200), 1024).is_err());
        let mut cmd = playbook::shell_free_command("node");
        cmd.args(["-e", "process.stdout.write('x'.repeat(4096))"]);
        assert!(process(cmd, &[], Duration::from_secs(5), 1024).is_err());
    }
    #[test]
    fn receiver_reconstructs_binary_snapshot_runs_once_and_refuses_cross_project() {
        let dir = Scratch::new().unwrap();
        let source = dir.0.join("source");
        let receiver = dir.0.join("receiver");
        std::fs::create_dir_all(source.join(".nitely")).unwrap();
        git(&source, &["init", "-q", "-b", "main"], &[], None).unwrap();
        std::fs::write(
            source.join(".nitely/playbook.toml"),
            "[lanes]\nquick=false\n[worktree]\ndir='../worktrees/{key}'\n",
        )
        .unwrap();
        std::fs::write(source.join("image.bin"), [0u8, 1, 255, 2]).unwrap();
        git(&source, &["add", "."], &[], None).unwrap();
        git(&source, &["commit", "-qm", "initial"], &[], None).unwrap();
        git(
            &dir.0,
            &[
                "clone",
                "--quiet",
                source.to_str().unwrap(),
                receiver.to_str().unwrap(),
            ],
            &[],
            None,
        )
        .unwrap();
        let base = git(&source, &["rev-parse", "HEAD"], &[], None).unwrap();
        let binary = [0u8, 255, 9, 8, 7, 0];
        std::fs::write(source.join("image.bin"), binary).unwrap();
        let snap = lanes::snapshot(&source).unwrap();
        let patch = git_bytes(
            &source,
            &["diff", "--binary", "--full-index", &base, &snap.commit],
            &[],
            None,
        )
        .unwrap();
        let request = WireRequest {
            version: 1,
            id: "binary-roundtrip".into(),
            project: "APP".into(),
            base,
            tree: snap.tree.clone(),
            patch: base64::engine::general_purpose::STANDARD.encode(patch),
            kind: Kind::Run,
            steps: vec![("echo executor-roundtrip".into(), None)],
        };
        let config = Config {
            repos: vec![crate::RepoConfig {
                name: "Acme".into(),
                root: receiver.to_string_lossy().into(),
                project: Some("APP".into()),
                integration_branch: "main".into(),
            }],
            ..Default::default()
        };
        let db = Db::open(&dir.0.join("data/nitely.db")).unwrap();
        let settings = Settings {
            receive_projects: vec!["APP".into()],
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(receive(&config, &db, &Settings::default(), "APP", &bytes).is_err());
        assert!(receive(&config, &db, &settings, "OTHER", &bytes).is_err());
        let first = receive(&config, &db, &settings, "APP", &bytes).unwrap();
        assert!(first.passed, "{}", serde_json::to_string(&first).unwrap());
        assert!(first.results[0].output.contains("executor-roundtrip"));
        let second = receive(&config, &db, &settings, "APP", &bytes).unwrap();
        assert_eq!(
            first.remote_run, second.remote_run,
            "retry returns receipt instead of executing twice"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM lane_run", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            git_bytes(
                &receiver,
                &["show", &format!("{}:image.bin", snap.tree)],
                &[],
                None
            )
            .unwrap(),
            binary
        );
        assert_eq!(
            std::fs::read(receiver.join("image.bin")).unwrap(),
            [0u8, 1, 255, 2],
            "main checkout untouched"
        );
        assert!(result(&db, &settings, "APP", &request.id).unwrap().passed);
        let mut changed = request;
        changed.steps[0].0 = "echo different".into();
        assert!(receive(
            &config,
            &db,
            &settings,
            "APP",
            &serde_json::to_vec(&changed).unwrap()
        )
        .is_err());
    }
    #[test]
    fn reply_requires_exact_identity_commands_and_explicit_fallback() {
        let r = WireRequest {
            version: 1,
            id: "request-1".into(),
            project: "APP".into(),
            base: "a".repeat(40),
            tree: "b".repeat(40),
            patch: "".into(),
            kind: Kind::Gate,
            steps: vec![("test".into(), None)],
        };
        let mut reply = r.unavailable("base_unavailable");
        assert!(validate_reply(&r, &reply).is_ok());
        reply.unavailable = Some("timeout".into());
        assert!(validate_reply(&r, &reply).is_err());
        reply.unavailable = None;
        reply.passed = true;
        reply.exit_code = Some(0);
        assert!(validate_reply(&r, &reply).is_err());
        reply.results.push(playbook::CommandResult {
            command: "test".into(),
            ok: true,
            seconds: 0,
            output: "".into(),
            skipped: false,
        });
        assert!(validate_reply(&r, &reply).is_ok());
        reply.tree = "c".repeat(40);
        assert!(validate_reply(&r, &reply).is_err());
        assert!(!safe_id("../escape"));
        assert!(!allowed(&["APP".into()], "OTHER"));
    }
    #[test]
    fn ci_requires_fresh_exact_sha_and_app_and_success() {
        let checks = vec![RequiredCheck {
            name: "test".into(),
            app_id: 42,
        }];
        let time = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut data = serde_json::json!({"check_runs":[{"id":1,"name":"test","app":{"id":42},"head_sha":"abc","started_at":"2026-01-01T00:00:00Z","status":"completed","conclusion":"success"}]});
        assert_eq!(checks_result(&data, &checks, "abc", time), Some(true));
        assert_eq!(checks_result(&data, &checks, "different", time), None);
        data["check_runs"][0]["app"]["id"] = serde_json::json!(1);
        assert_eq!(checks_result(&data, &checks, "abc", time), None);
        data["check_runs"][0]["app"]["id"] = serde_json::json!(42);
        data["check_runs"][0]["conclusion"] = serde_json::json!("skipped");
        assert_eq!(checks_result(&data, &checks, "abc", time), Some(false));
    }
}
