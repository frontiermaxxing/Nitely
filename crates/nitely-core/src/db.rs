use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub use crate::issues::IssueRecord;
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema from docs/design.md. `task`, `review` and `approval` are not written yet.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS project (
  key         TEXT PRIMARY KEY,            -- issue prefix, e.g. NIT
  name        TEXT NOT NULL,
  next_number INTEGER NOT NULL DEFAULT 1,
  created_at  INTEGER NOT NULL
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
  created_at TEXT NOT NULL DEFAULT '',
  updated_at TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS issue_project ON issue (project, state_type);
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
CREATE TABLE IF NOT EXISTS review (
  id          INTEGER PRIMARY KEY,
  task_id     INTEGER NOT NULL,
  patch_id    TEXT NOT NULL,
  reviewer    TEXT NOT NULL,
  verdict     TEXT NOT NULL,
  money_or_release INTEGER NOT NULL DEFAULT 0,
  sweeping    INTEGER NOT NULL DEFAULT 0,
  findings    TEXT NOT NULL DEFAULT '[]',
  ts          INTEGER NOT NULL
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
  PRIMARY KEY (provider, repo, number)
);
CREATE INDEX IF NOT EXISTS pull_request_branch ON pull_request (branch);
CREATE TABLE IF NOT EXISTS sync_status (
  source        TEXT PRIMARY KEY,
  last_attempt  INTEGER NOT NULL,
  last_ok       INTEGER,
  last_error    TEXT
);
CREATE TABLE IF NOT EXISTS approval (     -- written only from the app UI
  id        INTEGER PRIMARY KEY,
  issue_key TEXT NOT NULL,
  patch_id  TEXT NOT NULL,
  by        TEXT NOT NULL,
  ts        INTEGER NOT NULL
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
"#;

/// Bumped when a table changes shape. Tables that only mirror Bitbucket or Linear are dropped
/// and refilled by the next sync; tables Nitely owns get real migrations.
const SCHEMA_VERSION: i64 = 3;

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
    /// Approved by the account that owns the token (Andrew).
    pub approved_by_me: bool,
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

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        // WAL so the app reads while hooks write; busy_timeout so parallel hooks wait, not fail.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Db { conn })
    }

    pub fn open_default() -> Result<Db> {
        Db::open(&crate::paths::db_path())
    }

    /// Stores the event and moves the session's heartbeat, in one transaction.
    pub fn record(&mut self, e: &NewEvent) -> Result<()> {
        let ts = now();
        let tx = self.conn.transaction()?;
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
                   head_sha, updated_at, comment_count, approved_by, approved_by_me)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                    p.approved_by_me
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prs(&self) -> Result<Vec<PrRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, repo, number, title, url, state, branch, head_sha, updated_at,
                    comment_count, approved_by, approved_by_me
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
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
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
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_updates_session_and_keeps_last_activity() {
        let dir = std::env::temp_dir().join(format!("nitely-test-{}", std::process::id()));
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
        assert_eq!(s.last_activity.as_deref(), Some("Bash: npm test"));
        std::fs::remove_dir_all(dir).ok();
    }
}
