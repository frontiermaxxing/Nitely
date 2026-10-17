use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub use crate::issues::IssueRecord;
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema from docs/design.md. `task` and `approval` are not written yet.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS project (
  key         TEXT PRIMARY KEY,            -- issue prefix, e.g. NIT
  name        TEXT NOT NULL,
  next_number INTEGER NOT NULL DEFAULT 1,
  created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS project_alias (  -- a key a project had before; its old links still resolve
  old_key    TEXT PRIMARY KEY,
  project    TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS issue (
  key        TEXT PRIMARY KEY,
  origin     TEXT NOT NULL,                -- 'nitely' (native) or 'linear' (mirrored, read-only)
  project    TEXT,
  number     INTEGER,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',
  url        TEXT NOT NULL DEFAULT '',
  state      TEXT NOT NULL,                -- display name: 'In Progress', 'Needs You', ...
  state_type TEXT NOT NULL DEFAULT '',     -- backlog | unstarted | started | completed | canceled
  kind       TEXT,
  source     TEXT,
  priority   INTEGER NOT NULL DEFAULT 0,   -- 0 none, 1 urgent, 2 high, 3 medium, 4 low (Linear's scale)
  labels     TEXT NOT NULL DEFAULT '[]',
  evidence   TEXT NOT NULL DEFAULT '',
  claimed_by TEXT,
  claimed_at INTEGER,
  owner      TEXT NOT NULL DEFAULT 'agent', -- 'human' for work only a human can do; next_task skips it
  goal       INTEGER,                      -- the goal it belongs to, if any
  created_at TEXT NOT NULL DEFAULT '',
  updated_at TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS issue_project ON issue (project, state_type);
CREATE TABLE IF NOT EXISTS goal (          -- an outcome a project works toward, and its issues
  id          INTEGER PRIMARY KEY,
  project     TEXT NOT NULL,
  title       TEXT NOT NULL,
  outcome     TEXT NOT NULL,               -- one paragraph: what is true once it is reached
  target_date TEXT,                        -- YYYY-MM-DD
  state       TEXT NOT NULL DEFAULT 'active', -- active | done | dropped
  created_by  TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS goal_project ON goal (project, state);
CREATE TABLE IF NOT EXISTS goal_plan (     -- a goal and its issues an agent drafted; the person decides it
  id          INTEGER PRIMARY KEY,
  issue_key   TEXT NOT NULL,               -- the human-owned issue in Needs You that carries it
  project     TEXT NOT NULL,
  plan        TEXT NOT NULL,               -- JSON: sentence, title, outcome, target_date, issues, existing
  status      TEXT NOT NULL,               -- pending | approved | rejected
  created_by  TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  decided_by  TEXT,
  decided_at  INTEGER,
  outcome     TEXT,                        -- what approving filed, or why it was rejected
  goal_id     INTEGER                      -- the goal approving created
);
CREATE INDEX IF NOT EXISTS goal_plan_issue ON goal_plan (issue_key);
CREATE TABLE IF NOT EXISTS issue_blocker (  -- issue_key cannot start before blocker_key closes
  issue_key   TEXT NOT NULL,
  blocker_key TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (issue_key, blocker_key)
);
CREATE INDEX IF NOT EXISTS issue_blocker_blocker ON issue_blocker (blocker_key);
CREATE TABLE IF NOT EXISTS task (
  id            INTEGER PRIMARY KEY,
  issue_key     TEXT,
  repo          TEXT NOT NULL,
  branch        TEXT NOT NULL,
  worktree_path TEXT,
  vendor        TEXT,
  created_at    INTEGER NOT NULL,
  closed_at     INTEGER,
  UNIQUE (repo, branch)
);
CREATE TABLE IF NOT EXISTS session (
  id            TEXT PRIMARY KEY,          -- '<vendor>:<external id>'
  vendor        TEXT NOT NULL,
  cwd           TEXT NOT NULL,
  started_at    INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL,
  last_activity TEXT,
  state         TEXT NOT NULL              -- 'working' | 'idle' | 'waiting' | 'ended'
);
CREATE TABLE IF NOT EXISTS session_state ( -- one row each time a session's state changes
  session_id TEXT NOT NULL,
  ts         INTEGER NOT NULL,
  state      TEXT NOT NULL                 -- as session.state
);
CREATE INDEX IF NOT EXISTS session_state_session ON session_state (session_id, ts);
CREATE INDEX IF NOT EXISTS session_state_ts ON session_state (ts);
CREATE TABLE IF NOT EXISTS event (
  id         INTEGER PRIMARY KEY,
  ts         INTEGER NOT NULL,
  actor      TEXT NOT NULL,                -- session id, or 'human'
  cwd        TEXT,
  issue_key  TEXT,
  type       TEXT NOT NULL,
  summary    TEXT,
  payload    TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS event_ts ON event (ts);
CREATE INDEX IF NOT EXISTS event_issue ON event (issue_key, ts);
-- Lookups by issue key ignore case (`issue_key = ?1 COLLATE NOCASE`), which event_issue cannot serve.
CREATE INDEX IF NOT EXISTS event_issue_nocase ON event (issue_key COLLATE NOCASE, ts);
CREATE TABLE IF NOT EXISTS review (       -- one per submit; land checks the one for its patch
  id          INTEGER PRIMARY KEY,
  issue_key   TEXT NOT NULL,
  patch_id    TEXT NOT NULL,
  commit_sha  TEXT NOT NULL,
  reviewer    TEXT NOT NULL,             -- '' when no review ran
  verdict     TEXT NOT NULL,             -- pass | changes | none
  approver    TEXT NOT NULL,             -- agent | human
  reasons     TEXT NOT NULL DEFAULT '[]',
  findings    TEXT NOT NULL DEFAULT '[]',
  flagged     TEXT NOT NULL DEFAULT '[]',
  ts          INTEGER NOT NULL,
  note        TEXT NOT NULL DEFAULT ''   -- how independent: 'same vendor: codex not installed'
);
CREATE TABLE IF NOT EXISTS pull_request (
  provider       TEXT NOT NULL,
  repo           TEXT NOT NULL,            -- 'workspace/repo'
  number         INTEGER NOT NULL,
  title          TEXT NOT NULL,
  url            TEXT NOT NULL,
  state          TEXT NOT NULL,            -- OPEN | MERGED | DECLINED | SUPERSEDED
  branch         TEXT NOT NULL,
  head_sha       TEXT NOT NULL DEFAULT '',
  updated_at     TEXT NOT NULL DEFAULT '',
  comment_count  INTEGER NOT NULL DEFAULT 0,
  approved_by    TEXT NOT NULL DEFAULT '[]',
  approved_by_me INTEGER NOT NULL DEFAULT 0,
  checks         TEXT NOT NULL DEFAULT '',  -- success | failure | pending | '' (none or unknown)
  PRIMARY KEY (provider, repo, number)
);
CREATE INDEX IF NOT EXISTS pull_request_branch ON pull_request (branch);
CREATE TABLE IF NOT EXISTS sync_status (
  source        TEXT PRIMARY KEY,
  last_attempt  INTEGER NOT NULL,
  last_ok       INTEGER,
  last_error    TEXT
);
CREATE TABLE IF NOT EXISTS approval (     -- written only from the app UI, signed (docs/approvals.md)
  id        INTEGER PRIMARY KEY,
  issue_key TEXT NOT NULL,
  patch_id  TEXT NOT NULL,
  by        TEXT NOT NULL,
  ts        INTEGER NOT NULL,
  signature TEXT NOT NULL DEFAULT '',
  device    TEXT NOT NULL DEFAULT '',  -- a paired phone's id when the phone signed it (docs/phone.md)
  signed_at INTEGER                    -- the phone's own time in what it signed
);
CREATE TABLE IF NOT EXISTS mirror_project ( -- projects whose issues go to their repo's tracker (mirror.rs)
  project    TEXT PRIMARY KEY,
  enabled_at INTEGER NOT NULL,
  backfill   TEXT NOT NULL,             -- none, open or all: which existing issues it also publishes
  enabled_by TEXT
);
CREATE TABLE IF NOT EXISTS issue_mirror (   -- an issue's place in the mirror
  issue_key   TEXT PRIMARY KEY,
  status      TEXT NOT NULL,            -- mirrored, private, held, withdrawn or exposed (still public)
  reason      TEXT,
  host        TEXT,
  repo        TEXT,
  number      INTEGER,
  url         TEXT,
  fingerprint TEXT,                     -- of what was last sent, to send only changes
  last_event  INTEGER NOT NULL DEFAULT 0, -- the last timeline event considered for a comment
  left_on     TEXT,                     -- copies on repos the origin no longer points at, as #7 on GitHub o/r
  updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS mirror_comment ( -- comments the mirror posted, to take down if the issue goes private
  issue_key  TEXT NOT NULL,
  host_id    INTEGER NOT NULL,
  PRIMARY KEY (issue_key, host_id)
);
CREATE TABLE IF NOT EXISTS issue_private (  -- issues the person keeps off the host
  issue_key TEXT PRIMARY KEY,
  at        INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ask (
  id          INTEGER PRIMARY KEY,
  issue_key   TEXT,
  asked_by    TEXT NOT NULL,
  cwd         TEXT,
  question    TEXT NOT NULL,
  options     TEXT NOT NULL DEFAULT '[]',
  answer      TEXT,
  asked_at    INTEGER NOT NULL,
  answered_at INTEGER
);
CREATE TABLE IF NOT EXISTS project_brief ( -- what agents learned about a project, and when setup swept it
  project    TEXT PRIMARY KEY,
  brief      TEXT NOT NULL DEFAULT '',
  revision   INTEGER NOT NULL DEFAULT 0,   -- bumped on every write of brief
  updated_at INTEGER,
  updated_by TEXT,
  setup_sha  TEXT,                         -- the commit the setup sweep read; NULL until it ran
  setup_at   INTEGER,
  setup_by   TEXT
);
CREATE TABLE IF NOT EXISTS crew_worker (   -- one per agent process the crew started (docs/crew.md)
  id          INTEGER PRIMARY KEY,
  project     TEXT NOT NULL,
  issue_key   TEXT NOT NULL,
  agent       TEXT NOT NULL,             -- claude | codex
  agent_id    TEXT NOT NULL,             -- the id its MCP calls act as, e.g. 'codex:c:/x-worktrees/nit-3'
  pid         INTEGER,
  worktree    TEXT NOT NULL,
  log_path    TEXT NOT NULL,
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER,
  exit_code   INTEGER,
  outcome     TEXT,                      -- landed | submitted | asked | failed | stopped | launch_failed | setup_failed
  reason      TEXT,
  phase       TEXT NOT NULL DEFAULT 'work'  -- setup (its worktree is being set up) | work (the agent runs)
);
CREATE INDEX IF NOT EXISTS crew_worker_project ON crew_worker (project, started_at);
CREATE TABLE IF NOT EXISTS crew_control (  -- per project: paused, and the supervisor's heartbeat
  project        TEXT PRIMARY KEY,
  paused         INTEGER NOT NULL DEFAULT 0,
  changed_by     TEXT,
  changed_at     INTEGER,
  supervisor_pid INTEGER,
  heartbeat_at   INTEGER,
  status         TEXT,
  repo           TEXT                      -- the repo whose playbook the supervisor follows
);
CREATE TABLE IF NOT EXISTS playbook_proposal ( -- a playbook change an agent proposed; only a human applies it
  id         INTEGER PRIMARY KEY,
  issue_key  TEXT NOT NULL,                -- the issue in Needs You that carries it
  repo       TEXT NOT NULL,                -- the repo's name in Nitely's config
  root       TEXT NOT NULL,
  scope      TEXT NOT NULL,                -- the root, normalized: one pending proposal per repo
  in_repo    INTEGER NOT NULL,             -- 1: the repo commits .nitely/playbook.toml
  path       TEXT NOT NULL,                -- the file the playbook is read from
  steps      TEXT NOT NULL,                -- JSON [{mode, change}], earlier pending proposals first
  base       TEXT NOT NULL,                -- the playbook when proposed
  proposed   TEXT NOT NULL,                -- the playbook after every step
  why        TEXT NOT NULL,
  status     TEXT NOT NULL,                -- pending | applied | rejected | superseded
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  decided_by TEXT,
  decided_at INTEGER,
  outcome    TEXT                          -- what applying did, or why it was rejected
);
CREATE INDEX IF NOT EXISTS playbook_proposal_issue ON playbook_proposal (issue_key);
CREATE UNIQUE INDEX IF NOT EXISTS playbook_proposal_pending ON playbook_proposal (scope) WHERE status = 'pending';
"#;

/// Bumped when a table changes shape. Tables that only mirror Bitbucket or Linear are dropped
/// and refilled by the next sync; tables Nitely owns get real migrations.
const SCHEMA_VERSION: i64 = 22;

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 2 {
        // v1 (first scaffold) had placeholder issue and pull_request tables that were never written.
        conn.execute_batch("DROP TABLE IF EXISTS issue; DROP TABLE IF EXISTS pull_request;")?;
    }
    if version < 3 {
        // v2's issue table only mirrored Linear (refilled by the next sync); ask was never written.
        conn.execute_batch("DROP TABLE IF EXISTS issue; DROP TABLE IF EXISTS ask;")?;
    }
    if version < 4 {
        // v4 adds pull_request.checks; the table only mirrors code hosts.
        conn.execute_batch("DROP TABLE IF EXISTS pull_request;")?;
    }
    if version < 5 {
        // v5 keys reviews by issue and patch; the old placeholder table was never written.
        conn.execute_batch("DROP TABLE IF EXISTS review;")?;
    }
    // v6 adds issue_blocker, v7 session_state, v8 project_alias, v9 project_brief and v10 crew_worker
    // and crew_control, which SCHEMA creates; nothing to drop.
    if version < 11 {
        // v11 adds review.note; reviews are Nitely's own records, so the column is added, not refilled.
        let has_review: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'review')", [], |r| r.get(0))?;
        let has_note: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM pragma_table_info('review') WHERE name = 'note')", [], |r| r.get(0))?;
        if has_review && !has_note {
            conn.execute_batch("ALTER TABLE review ADD COLUMN note TEXT NOT NULL DEFAULT '';")?;
        }
    }
    // v12 adds issue.owner; v13 adds playbook_proposal, which SCHEMA creates.
    add_column(conn, "issue", "owner", "TEXT NOT NULL DEFAULT 'agent'")?;
    // v14 adds the event_issue_nocase index, which SCHEMA creates.
    // v15 adds crew_worker.phase: a worker's worktree setup is recorded too, so a new supervisor finds it.
    add_column(conn, "crew_worker", "phase", "TEXT NOT NULL DEFAULT 'work'")?;
    // v16 adds crew_control.repo: the repo whose playbook a project's supervisor follows.
    add_column(conn, "crew_control", "repo", "TEXT")?;
    // v17 adds approval.signature.
    add_column(conn, "approval", "signature", "TEXT NOT NULL DEFAULT ''")?;
    // v18 adds mirror_project, issue_mirror, mirror_comment and issue_private, which SCHEMA creates.
    // v19 adds issue_mirror.left_on: copies on repos the origin no longer points at.
    add_column(conn, "issue_mirror", "left_on", "TEXT")?;
    // v20 adds the goal table, which SCHEMA creates, and issue.goal.
    add_column(conn, "issue", "goal", "INTEGER")?;
    // v21 adds the goal_plan table, which SCHEMA creates.
    // v22 adds approval.device and approval.signed_at: approvals a paired phone signed.
    add_column(conn, "approval", "device", "TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "approval", "signed_at", "INTEGER")?;
    Ok(())
}

/// Puts the database in WAL mode (persistent), so the app reads while hooks write. SQLite refuses
/// the switch as "locked" while another connection has the file open, without waiting for it as
/// busy_timeout does for writes, so a few processes opening a new database at once each retry
/// until one has switched it (NIT-103).
fn use_wal(conn: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) => anyhow::bail!("the database stays in {mode} mode"),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Brings the schema up to SCHEMA_VERSION in one write transaction. The app, a hook and an MCP
/// server often open an old database at the same moment after an install; the first takes the
/// write lock and upgrades, the others wait for it (busy_timeout), then find the version current
/// and do nothing, instead of each adding the same column (NIT-103).
fn upgrade(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let done = (|| -> Result<()> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < SCHEMA_VERSION {
            migrate(conn)?;
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    })();
    match done {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            conn.execute_batch("ROLLBACK").ok();
            return Err(e);
        }
    }
    Ok(())
}

/// Adds a column to a table that exists without it. SCHEMA creates new tables whole.
fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let columns: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if !columns.is_empty() && !columns.iter().any(|c| c == column) {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct Db {
    pub(crate) conn: Connection,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub vendor: String,
    pub cwd: String,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub last_activity: Option<String>,
    pub state: String,
    /// The crew started it: it runs in a crew worker's worktree, since that worker started.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub crew: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrRecord {
    pub provider: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub branch: String,
    pub head_sha: String,
    pub updated_at: String,
    pub comment_count: i64,
    pub approved_by: Vec<String>,
    /// Approved by the account that owns the token (you).
    pub approved_by_me: bool,
    /// CI on the head commit: success, failure, pending, or empty when unknown.
    pub checks: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncStatus {
    pub source: String,
    pub last_attempt: i64,
    pub last_ok: Option<i64>,
    pub last_error: Option<String>,
}

/// One hook call or CLI report, before it is stored.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub session_id: String,
    pub vendor: String,
    pub cwd: String,
    pub kind: String,
    pub summary: Option<String>,
    /// Session state after this event: `working`, `idle` (turn finished), `waiting` (needs a
    /// human, e.g. a permission prompt) or `ended`.
    pub state: String,
}

/// Beside the database: the version of Nitely that last upgraded its schema.
fn upgraded_by_path(db: &Path) -> std::path::PathBuf {
    let mut name = db.file_name().unwrap_or_default().to_os_string();
    name.push(".upgraded-by");
    db.with_file_name(name)
}

/// What an older build says when it meets a database a newer one upgraded (an update from a newer
/// channel to an older one): it does not open it, so it can never write in a form the newer build
/// misreads.
fn newer_database(path: &Path, version: i64) -> String {
    let by = std::fs::read_to_string(upgraded_by_path(path)).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match by {
        Some(v) => format!("this database was upgraded by Nitely {v}; this is Nitely {}. Install {v} or newer", crate::update::VERSION),
        None => format!("this database was upgraded by a newer Nitely (schema {version}; this build knows {SCHEMA_VERSION}); install the newer version"),
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Only an outdated database is written to here. Every app command opens the database, and
        // a write on open moves `data_version`, which the app's watcher reads as a change: it would
        // refresh, open the database again, and loop.
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!("{}", newer_database(path, version));
        }
        if version < SCHEMA_VERSION {
            // WAL (persistent) so the app reads while hooks write; busy_timeout so parallel hooks wait.
            use_wal(&conn).context("switching the database to WAL")?;
            upgrade(&conn).context("upgrading the database")?;
            // Who upgraded it, so an older build can say which version to install (`newer_database`).
            std::fs::write(upgraded_by_path(path), crate::update::VERSION).ok();
        }
        Ok(Db { conn })
    }

    pub fn open_default() -> Result<Db> {
        Db::open(&crate::paths::db_path())
    }

    /// Opens the database so that SQLite itself refuses every write: no file is created, nothing is
    /// migrated, and any write a read path might try fails. For `nitely ui`, the browser preview's
    /// live data, which must never change Nitely. An outdated database fails to read instead of
    /// being migrated; the app migrates it on its next start.
    pub fn open_read_only(path: &Path) -> Result<Db> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Db { conn })
    }

    /// Stores the event and moves the session's heartbeat, in one transaction. A change of state
    /// (or the first state seen for a session) is also kept in `session_state`, so the Activity
    /// view can show what each session did over time.
    pub fn record(&mut self, e: &NewEvent) -> Result<()> {
        let ts = now();
        // Immediate: the read of the last state and the writes after it hold one write lock, so
        // a hook writing at the same moment waits (busy_timeout) instead of failing this one.
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let last: Option<String> = tx
            .query_row(
                "SELECT state FROM session_state WHERE session_id = ?1 ORDER BY ts DESC, rowid DESC LIMIT 1",
                [&e.session_id],
                |r| r.get(0),
            )
            .optional()?;
        if last.as_deref() != Some(e.state.as_str()) {
            tx.execute(
                "INSERT INTO session_state (session_id, ts, state) VALUES (?1, ?2, ?3)",
                params![e.session_id, ts, e.state],
            )?;
        }
        tx.execute(
            "INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, last_activity, state)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
             ON CONFLICT (id) DO UPDATE SET
               cwd = excluded.cwd,
               last_seen_at = excluded.last_seen_at,
               last_activity = COALESCE(excluded.last_activity, session.last_activity),
               state = excluded.state",
            params![
                e.session_id,
                e.vendor,
                e.cwd,
                ts,
                e.summary,
                e.state
            ],
        )?;
        tx.execute(
            "INSERT INTO event (ts, actor, cwd, type, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, e.session_id, e.cwd, e.kind, e.summary],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces the stored pull requests of one remote repo.
    pub fn replace_prs(&mut self, provider: &str, repo: &str, prs: &[PrRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM pull_request WHERE provider = ?1 AND repo = ?2", [provider, repo])?;
        {
            let mut ins = tx.prepare(
                "INSERT OR REPLACE INTO pull_request (provider, repo, number, title, url, state, branch,
                   head_sha, updated_at, comment_count, approved_by, approved_by_me, checks)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for p in prs {
                ins.execute(params![
                    provider,
                    repo,
                    p.number,
                    p.title,
                    p.url,
                    p.state,
                    p.branch,
                    p.head_sha,
                    p.updated_at,
                    p.comment_count,
                    serde_json::to_string(&p.approved_by)?,
                    p.approved_by_me,
                    p.checks
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prs(&self) -> Result<Vec<PrRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, repo, number, title, url, state, branch, head_sha, updated_at,
                    comment_count, approved_by, approved_by_me, checks
             FROM pull_request ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PrRecord {
                provider: r.get(0)?,
                repo: r.get(1)?,
                number: r.get(2)?,
                title: r.get(3)?,
                url: r.get(4)?,
                state: r.get(5)?,
                branch: r.get(6)?,
                head_sha: r.get(7)?,
                updated_at: r.get(8)?,
                comment_count: r.get(9)?,
                approved_by: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
                approved_by_me: r.get(11)?,
                checks: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Pull requests whose branch names this issue (`claude/nit-7-...` for NIT-7), newest first.
    pub fn prs_for_issue(&self, key: &str) -> Result<Vec<PrRecord>> {
        // A pull request opened before its project changed key names the old one in its branch.
        let aliases = self.project_aliases()?;
        Ok(self
            .prs()?
            .into_iter()
            .filter(|p| {
                crate::git::parse_branch(&p.branch)
                    .1
                    .is_some_and(|k| crate::projects::current_key(&aliases, &k).eq_ignore_ascii_case(key))
            })
            .collect())
    }

    pub fn set_sync_status(&self, source: &str, error: Option<&str>) -> Result<()> {
        let ts = now();
        self.conn.execute(
            "INSERT INTO sync_status (source, last_attempt, last_ok, last_error) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (source) DO UPDATE SET
               last_attempt = excluded.last_attempt,
               last_ok = COALESCE(excluded.last_ok, sync_status.last_ok),
               last_error = excluded.last_error",
            params![source, ts, if error.is_none() { Some(ts) } else { None }, error],
        )?;
        Ok(())
    }

    pub fn sync_statuses(&self) -> Result<Vec<SyncStatus>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source, last_attempt, last_ok, last_error FROM sync_status ORDER BY source")?;
        let rows = stmt.query_map([], |r| {
            Ok(SyncStatus { source: r.get(0)?, last_attempt: r.get(1)?, last_ok: r.get(2)?, last_error: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn sessions_since(&self, since: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, vendor, cwd, started_at, last_seen_at, last_activity, state
             FROM session WHERE last_seen_at >= ?1 ORDER BY last_seen_at DESC",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(Session {
                id: r.get(0)?,
                vendor: r.get(1)?,
                cwd: r.get(2)?,
                started_at: r.get(3)?,
                last_seen_at: r.get(4)?,
                last_activity: r.get(5)?,
                state: r.get(6)?,
                crew: false,
            })
        })?;
        let mut sessions: Vec<Session> = rows.collect::<rusqlite::Result<_>>()?;
        self.mark_crew(&mut sessions);
        Ok(sessions)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        let session = self
            .conn
            .query_row(
                "SELECT id, vendor, cwd, started_at, last_seen_at, last_activity, state
                 FROM session WHERE id = ?1",
                [id],
                |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        vendor: r.get(1)?,
                        cwd: r.get(2)?,
                        started_at: r.get(3)?,
                        last_seen_at: r.get(4)?,
                        last_activity: r.get(5)?,
                        state: r.get(6)?,
                        crew: false,
                    })
                },
            )
            .optional()?;
        let mut sessions: Vec<Session> = session.into_iter().collect();
        self.mark_crew(&mut sessions);
        Ok(sessions.pop())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_build_refuses_a_database_a_newer_one_upgraded() {
        let dir = crate::paths::unique_temp("nitely-newer", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        drop(Db::open(&path).unwrap());
        assert_eq!(std::fs::read_to_string(upgraded_by_path(&path)).unwrap(), crate::update::VERSION, "the upgrade names its version");
        Connection::open(&path).unwrap().pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        std::fs::write(upgraded_by_path(&path), "9.9.0-nightly.20261016.g1a2b3c4").unwrap();
        let err = Db::open(&path).err().unwrap().to_string();
        assert!(err.contains("upgraded by Nitely 9.9.0-nightly.20261016.g1a2b3c4") && err.contains("Install 9.9.0-nightly"), "{err}");
        std::fs::remove_file(upgraded_by_path(&path)).unwrap();
        let err = Db::open(&path).err().unwrap().to_string();
        assert!(err.contains(&format!("schema {}", SCHEMA_VERSION + 1)), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_v8_database_gets_issue_owners() {
        let dir = crate::paths::unique_temp("nitely-v8", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&SCHEMA.replace("  owner      TEXT NOT NULL DEFAULT 'agent', -- 'human' for work only a human can do; next_task skips it
", "")).unwrap();
            conn.execute_batch("INSERT INTO issue (key, origin, project, number, title, state) VALUES ('NIT-1', 'nitely', 'NIT', 1, 'Old', 'Todo');").unwrap();
            conn.pragma_update(None, "user_version", 8).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().owner, "agent");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn processes_opening_an_old_database_together_upgrade_it_once() {
        // As the app, a hook and an MCP server do right after an install (NIT-103): each opens
        // its own connection to a v10 database whose review table has no note column yet.
        for round in 0..8 {
            let dir = crate::paths::unique_temp("nitely-upgrade", "");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("t.db");
            {
                let conn = Connection::open(&path).unwrap();
                let old = SCHEMA.replace(",\n  note        TEXT NOT NULL DEFAULT ''", "\n  --");
                conn.execute_batch(&old).unwrap();
                assert!(!conn.query_row("SELECT EXISTS (SELECT 1 FROM pragma_table_info('review') WHERE name = 'note')", [], |r| r.get::<_, bool>(0)).unwrap());
                conn.pragma_update(None, "user_version", 10).unwrap();
            }
            let start = std::sync::Arc::new(std::sync::Barrier::new(6));
            let opened: Vec<_> = (0..6)
                .map(|_| {
                    let (path, start) = (path.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        Db::open(&path).map(|_| ()).map_err(|e| format!("{e:#}"))
                    })
                })
                .collect();
            for o in opened {
                assert_eq!(o.join().unwrap(), Ok(()), "round {round}");
            }
            let db = Db::open(&path).unwrap();
            assert_eq!(db.conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).unwrap(), SCHEMA_VERSION);
            drop(db);
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn record_updates_session_and_keeps_last_activity() {
        // Its own folder per run: Windows reuses process ids, and a database left by an earlier run
        // with the same id doubled the state changes counted below.
        let dir = crate::paths::unique_temp("nitely-test", "");
        std::fs::remove_dir_all(&dir).ok();
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        let mut e = NewEvent {
            session_id: "claude:abc".into(),
            vendor: "claude".into(),
            cwd: "/w/and-1".into(),
            kind: "tool".into(),
            summary: Some("Bash: npm test".into()),
            state: "working".into(),
        };
        db.record(&e).unwrap();
        e.kind = "stop".into();
        e.summary = None;
        e.state = "idle".into();
        db.record(&e).unwrap();
        let s = db.session("claude:abc").unwrap().unwrap();
        assert_eq!(s.state, "idle");
        // Two events, two states: both changes are kept; a repeat of the same state is not.
        db.record(&e).unwrap();
        let changes: i64 = db.conn.query_row("SELECT COUNT(*) FROM session_state WHERE session_id = 'claude:abc'", [], |r| r.get(0)).unwrap();
        assert_eq!(changes, 2);
        assert_eq!(s.last_activity.as_deref(), Some("Bash: npm test"));
        std::fs::remove_dir_all(dir).ok();
    }
}
