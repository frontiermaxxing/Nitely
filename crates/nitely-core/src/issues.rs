//! Projects and the native backlog. The rules are the ones `scripts/work/linear.mjs` enforces in the
//! Aera workflow, so agents file and move issues the same way they already do: a kind and a source
//! on every issue, evidence to file or finish one, a reason to cancel, a duplicate check before
//! filing, and Approved reserved for a human.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use std::collections::HashSet;

use crate::db::{now, Db};

pub const KINDS: &[&str] = &["bug", "feature", "improvement", "idea", "live-check"];
pub const SOURCES: &[&str] = &["claude", "codex", "cursor", "human"];

/// Native workflow states: (display name, Linear-style type).
pub const STATES: &[(&str, &str)] = &[
    ("Backlog", "backlog"),
    ("Todo", "unstarted"),
    ("In Progress", "started"),
    ("In Review", "started"),
    ("Needs You", "started"),
    ("Approved", "started"),
    ("Done", "completed"),
    ("Canceled", "canceled"),
    ("Duplicate", "canceled"),
];

/// States that mean the next step is the human's. "Needs Andrew" is the Aera Linear name.
pub const NEEDS_HUMAN: &[&str] = &["Needs You", "Needs Andrew"];

/// Titles sharing this share of their words count as possible duplicates (`linear.mjs`: 60%).
const SIMILAR_TITLE: f64 = 0.6;

/// A claim older than this with no update is free for another agent to take.
const CLAIM_TTL_SECS: i64 = 6 * 3600;

#[derive(Debug, Clone, Default, Serialize)]
pub struct IssueRecord {
    pub key: String,
    /// `nitely` for native issues, `linear` for the read-only mirror.
    pub origin: String,
    pub project: Option<String>,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    /// backlog, unstarted, started, completed or canceled.
    pub state_type: String,
    pub kind: Option<String>,
    pub source: Option<String>,
    /// 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    pub priority: i64,
    pub labels: Vec<String>,
    pub evidence: String,
    pub claimed_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl IssueRecord {
    pub fn is_open(&self) -> bool {
        !matches!(self.state_type.as_str(), "completed" | "canceled")
    }

    pub fn needs_human(&self) -> bool {
        NEEDS_HUMAN.contains(&self.state.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub key: String,
    pub name: String,
    pub open_issues: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineEntry {
    pub ts: i64,
    pub actor: String,
    pub kind: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ask {
    pub id: i64,
    pub issue_key: Option<String>,
    pub asked_by: String,
    pub question: String,
    pub options: Vec<String>,
    pub answer: Option<String>,
    pub asked_at: i64,
    pub answered_at: Option<i64>,
}

/// Who is acting. Only a human may approve or answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Human,
    Agent(String),
}

impl Actor {
    pub fn id(&self) -> &str {
        match self {
            Actor::Human => "human",
            Actor::Agent(id) => id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewIssue {
    pub project: String,
    pub title: String,
    pub body: String,
    pub kind: String,
    pub source: String,
    pub evidence: String,
    pub priority: i64,
    /// File even when an open issue has a similar title.
    pub allow_similar: bool,
    /// Defaults to Todo, so `next_task` can hand it out.
    pub state: Option<String>,
}

const ISSUE_COLUMNS: &str = "key, origin, project, title, body, url, state, state_type, kind, source,
    priority, labels, evidence, claimed_by, created_at, updated_at";

fn issue_from_row(r: &Row) -> rusqlite::Result<IssueRecord> {
    Ok(IssueRecord {
        key: r.get(0)?,
        origin: r.get(1)?,
        project: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        url: r.get(5)?,
        state: r.get(6)?,
        state_type: r.get(7)?,
        kind: r.get(8)?,
        source: r.get(9)?,
        priority: r.get(10)?,
        labels: serde_json::from_str(&r.get::<_, String>(11)?).unwrap_or_default(),
        evidence: r.get(12)?,
        claimed_by: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

/// Accepts "in progress", "in-progress", "IN_PROGRESS", and "Needs Andrew" for Needs You.
pub fn parse_state(input: &str) -> Result<(&'static str, &'static str)> {
    let norm = |s: &str| s.to_lowercase().replace(['-', '_'], " ");
    let want = norm(input.trim());
    let want = if want == "needs andrew" { "needs you".to_string() } else { want };
    STATES
        .iter()
        .find(|(name, _)| norm(name) == want)
        .copied()
        .with_context(|| {
            let names: Vec<&str> = STATES.iter().map(|(n, _)| *n).collect();
            format!("unknown state {input:?}; one of: {}", names.join(", "))
        })
}

pub fn title_similarity(a: &str, b: &str) -> f64 {
    let words = |s: &str| -> HashSet<String> {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == ' ' { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .map(str::to_string)
            .collect()
    };
    let (x, y) = (words(a), words(b));
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    x.intersection(&y).count() as f64 / x.len().min(y.len()) as f64
}

/// RFC 3339 UTC from unix seconds (days-from-civil, Howard Hinnant's algorithm).
pub fn iso(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// `claude/nit-12-fix-sidebar`: the branch name the Aera workflow uses for an issue.
pub fn branch_name(vendor: &str, key: &str, title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|w| !w.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    format!("{vendor}/{}-{slug}", key.to_lowercase())
}

impl Db {
    // ----- projects -----

    pub fn create_project(&self, key: &str, name: &str) -> Result<Project> {
        let key = key.trim().to_uppercase();
        let valid = (2..=10).contains(&key.len())
            && key.starts_with(|c: char| c.is_ascii_uppercase())
            && key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if !valid {
            bail!("project key must be 2-10 letters or digits, starting with a letter (e.g. NIT)");
        }
        let taken: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM project WHERE key = ?1)
                 OR EXISTS (SELECT 1 FROM issue WHERE key LIKE ?1 || '-%')",
            [&key],
            |r| r.get(0),
        )?;
        if taken {
            bail!("{key} is already a project or an issue prefix");
        }
        self.conn.execute(
            "INSERT INTO project (key, name, created_at) VALUES (?1, ?2, ?3)",
            params![key, name.trim(), now()],
        )?;
        Ok(Project { key, name: name.trim().into(), open_issues: 0 })
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.key, p.name,
               (SELECT COUNT(*) FROM issue i WHERE i.project = p.key
                  AND i.state_type NOT IN ('completed', 'canceled'))
             FROM project p ORDER BY p.key",
        )?;
        let rows = stmt.query_map([], |r| Ok(Project { key: r.get(0)?, name: r.get(1)?, open_issues: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, key: &str) -> Result<Option<Project>> {
        Ok(self.projects()?.into_iter().find(|p| p.key.eq_ignore_ascii_case(key)))
    }

    // ----- issues -----

    pub fn create_issue(&mut self, new: &NewIssue, actor: &Actor) -> Result<IssueRecord> {
        let project = self
            .project(&new.project)?
            .with_context(|| format!("no project {}; create it with `nitely project create`", new.project))?;
        let title = new.title.trim();
        if title.is_empty() {
            bail!("an issue needs a title");
        }
        if new.body.trim().is_empty() {
            bail!("an issue needs a body: what is wrong or wanted, and why");
        }
        if new.evidence.trim().is_empty() {
            bail!("an issue needs evidence: what you saw or ran, or \"none yet\" if it is an idea");
        }
        if !KINDS.contains(&new.kind.as_str()) {
            bail!("kind must be one of {}", KINDS.join(", "));
        }
        let source = if new.source == "andrew" { "human" } else { new.source.as_str() };
        if !SOURCES.contains(&source) {
            bail!("source must be one of {}", SOURCES.join(", "));
        }
        if !(0..=4).contains(&new.priority) {
            bail!("priority is 0 (none), 1 (urgent), 2 (high), 3 (medium) or 4 (low)");
        }
        let (state, state_type) = parse_state(new.state.as_deref().unwrap_or("Todo"))?;
        if state == "Approved" {
            bail!("an issue cannot be filed as Approved");
        }

        if !new.allow_similar {
            let similar: Vec<String> = self
                .list_issues(Some(&project.key), false)?
                .into_iter()
                .filter(|i| title_similarity(title, &i.title) >= SIMILAR_TITLE)
                .map(|i| format!("{} {}", i.key, i.title))
                .collect();
            if !similar.is_empty() {
                bail!(
                    "possible duplicates (file anyway with allow_similar):\n  {}",
                    similar.join("\n  ")
                );
            }
        }

        let ts = now();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let number: i64 = tx.query_row(
            "UPDATE project SET next_number = next_number + 1 WHERE key = ?1 RETURNING next_number - 1",
            [&project.key],
            |r| r.get(0),
        )?;
        let key = format!("{}-{number}", project.key);
        tx.execute(
            "INSERT INTO issue (key, origin, project, number, title, body, state, state_type, kind, source,
               priority, evidence, created_at, updated_at)
             VALUES (?1, 'nitely', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![
                key,
                project.key,
                number,
                title,
                new.body.trim(),
                state,
                state_type,
                new.kind,
                source,
                new.priority,
                new.evidence.trim(),
                iso(ts)
            ],
        )?;
        tx.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'issue_created', ?4)",
            params![ts, actor.id(), key, format!("Filed as {state}")],
        )?;
        tx.commit()?;
        self.issue(&key)?.context("issue vanished after insert")
    }

    pub fn issue(&self, key: &str) -> Result<Option<IssueRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ISSUE_COLUMNS} FROM issue WHERE key = ?1 COLLATE NOCASE"),
                [key],
                issue_from_row,
            )
            .optional()?)
    }

    /// Every issue (native and mirrored); `project` narrows to one project's native issues.
    pub fn list_issues(&self, project: Option<&str>, include_closed: bool) -> Result<Vec<IssueRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ISSUE_COLUMNS} FROM issue
             WHERE (?1 IS NULL OR project = ?1 COLLATE NOCASE)
               AND (?2 OR state_type NOT IN ('completed', 'canceled'))
             ORDER BY CASE priority WHEN 0 THEN 5 ELSE priority END, number, key"
        ))?;
        let rows = stmt.query_map(params![project, include_closed], issue_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Kept for the snapshot and older callers: every stored issue, open or not.
    pub fn issues(&self) -> Result<Vec<IssueRecord>> {
        self.list_issues(None, true)
    }

    /// Moves a native issue. Done needs evidence, Canceled and Duplicate a reason, and only a human
    /// may set Approved. Leaving In Progress releases the claim.
    pub fn move_issue(
        &mut self,
        key: &str,
        to: &str,
        evidence: Option<&str>,
        reason: Option<&str>,
        actor: &Actor,
    ) -> Result<IssueRecord> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.origin != "nitely" {
            bail!("{} is mirrored from {}; change it there", issue.key, issue.origin);
        }
        let (state, state_type) = parse_state(to)?;
        if state == "Approved" && *actor != Actor::Human {
            bail!("only a human can approve; ask for approval with `ask` and move the issue to Needs You");
        }
        let evidence = evidence.map(str::trim).filter(|e| !e.is_empty());
        let reason = reason.map(str::trim).filter(|r| !r.is_empty());
        if state == "Done" && evidence.is_none() {
            bail!("moving to Done needs evidence: what ran and what it showed");
        }
        if matches!(state, "Canceled" | "Duplicate") && reason.is_none() {
            bail!("moving to {state} needs a reason");
        }

        let ts = now();
        let summary = match (evidence, reason) {
            (Some(e), _) => format!("{} → {state}: {e}", issue.state),
            (None, Some(r)) => format!("{} → {state}: {r}", issue.state),
            _ => format!("{} → {state}", issue.state),
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE issue SET state = ?2, state_type = ?3, updated_at = ?4,
               evidence = CASE WHEN ?5 IS NULL THEN evidence ELSE ?5 END,
               claimed_by = CASE WHEN ?2 IN ('In Progress', 'In Review') THEN claimed_by ELSE NULL END,
               claimed_at = CASE WHEN ?2 IN ('In Progress', 'In Review') THEN claimed_at ELSE NULL END
             WHERE key = ?1",
            params![issue.key, state, state_type, iso(ts), evidence],
        )?;
        tx.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'issue_moved', ?4)",
            params![ts, actor.id(), issue.key, summary],
        )?;
        tx.commit()?;
        self.issue(&issue.key)?.context("issue vanished")
    }

    /// Claims the most urgent Todo issue of a project for `agent`, atomically: two agents asking at
    /// once get different issues. A claim untouched for `CLAIM_TTL_SECS` counts as abandoned.
    pub fn next_task(&mut self, project: &str, agent: &str) -> Result<Option<IssueRecord>> {
        let ts = now();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key: Option<String> = tx
            .query_row(
                "SELECT key FROM issue
                 WHERE project = ?1 COLLATE NOCASE AND origin = 'nitely'
                   AND (state = 'Todo' OR (state = 'In Progress' AND claimed_at < ?2))
                 ORDER BY CASE priority WHEN 0 THEN 5 ELSE priority END, number
                 LIMIT 1",
                params![project, ts - CLAIM_TTL_SECS],
                |r| r.get(0),
            )
            .optional()?;
        let Some(key) = key else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE issue SET state = 'In Progress', state_type = 'started', claimed_by = ?2,
               claimed_at = ?3, updated_at = ?4 WHERE key = ?1",
            params![key, agent, ts, iso(ts)],
        )?;
        tx.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'claimed', 'Took the issue')",
            params![ts, agent, key],
        )?;
        tx.commit()?;
        self.issue(&key)
    }

    /// A progress line, handoff note or comment on an issue's timeline. Refreshes a claim it holds.
    pub fn add_note(&self, key: &str, text: &str, kind: &str, actor: &Actor) -> Result<()> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if text.trim().is_empty() {
            bail!("a note needs text");
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, actor.id(), issue.key, kind, text.trim()],
        )?;
        self.conn.execute(
            "UPDATE issue SET claimed_at = ?3 WHERE key = ?1 AND claimed_by = ?2",
            params![issue.key, actor.id(), ts],
        )?;
        Ok(())
    }

    pub fn timeline(&self, key: &str) -> Result<Vec<TimelineEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, actor, type, summary FROM event WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts, id",
        )?;
        let rows = stmt.query_map([key], |r| {
            Ok(TimelineEntry { ts: r.get(0)?, actor: r.get(1)?, kind: r.get(2)?, summary: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces every issue from `origin` (the Linear mirror) with `issues`, in one transaction.
    pub fn replace_issues(&mut self, origin: &str, issues: &[IssueRecord]) -> Result<()> {
        if origin == "nitely" {
            bail!("native issues are never bulk-replaced");
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM issue WHERE origin = ?1", [origin])?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO issue (key, origin, title, url, state, state_type, labels, updated_at, priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for i in issues {
                ins.execute(params![
                    i.key,
                    origin,
                    i.title,
                    i.url,
                    i.state,
                    i.state_type,
                    serde_json::to_string(&i.labels)?,
                    i.updated_at,
                    i.priority
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- asks: an agent's question for the human -----

    pub fn create_ask(
        &self,
        question: &str,
        options: &[String],
        issue_key: Option<&str>,
        cwd: Option<&str>,
        actor: &Actor,
    ) -> Result<Ask> {
        if question.trim().is_empty() {
            bail!("an ask needs a question");
        }
        let issue_key = match issue_key {
            Some(k) => Some(self.issue(k)?.with_context(|| format!("no issue {k}"))?.key),
            None => None,
        };
        let ts = now();
        self.conn.execute(
            "INSERT INTO ask (issue_key, asked_by, cwd, question, options, asked_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![issue_key, actor.id(), cwd, question.trim(), serde_json::to_string(options)?, ts],
        )?;
        let id = self.conn.last_insert_rowid();
        if let Some(k) = &issue_key {
            self.conn.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'ask', ?4)",
                params![ts, actor.id(), k, question.trim()],
            )?;
        }
        self.ask(id)?.context("ask vanished")
    }

    pub fn ask(&self, id: i64) -> Result<Option<Ask>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, issue_key, asked_by, question, options, answer, asked_at, answered_at FROM ask WHERE id = ?1",
                [id],
                ask_from_row,
            )
            .optional()?)
    }

    pub fn open_asks(&self) -> Result<Vec<Ask>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, issue_key, asked_by, question, options, answer, asked_at, answered_at
             FROM ask WHERE answer IS NULL ORDER BY asked_at",
        )?;
        let rows = stmt.query_map([], ask_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn answer_ask(&self, id: i64, answer: &str, actor: &Actor) -> Result<Ask> {
        if *actor != Actor::Human {
            bail!("only a human answers asks");
        }
        let ask = self.ask(id)?.with_context(|| format!("no ask {id}"))?;
        if ask.answer.is_some() {
            bail!("ask {id} is already answered");
        }
        let ts = now();
        self.conn.execute(
            "UPDATE ask SET answer = ?2, answered_at = ?3 WHERE id = ?1",
            params![id, answer.trim(), ts],
        )?;
        if let Some(k) = &ask.issue_key {
            self.conn.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', ?2, 'answer', ?3)",
                params![ts, k, answer.trim()],
            )?;
        }
        self.ask(id)?.context("ask vanished")
    }
}

fn ask_from_row(r: &Row) -> rusqlite::Result<Ask> {
    Ok(Ask {
        id: r.get(0)?,
        issue_key: r.get(1)?,
        asked_by: r.get(2)?,
        question: r.get(3)?,
        options: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        answer: r.get(5)?,
        asked_at: r.get(6)?,
        answered_at: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "nitely-issues-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        (Db::open(&dir.join("t.db")).unwrap(), dir)
    }

    fn new(title: &str) -> NewIssue {
        NewIssue {
            project: "NIT".into(),
            title: title.into(),
            body: "why".into(),
            kind: "feature".into(),
            source: "claude".into(),
            evidence: "none yet".into(),
            ..Default::default()
        }
    }

    #[test]
    fn numbers_issues_per_project_and_blocks_duplicates() {
        let (mut db, dir) = db();
        db.create_project("nit", "Nitely").unwrap();
        let agent = Actor::Agent("claude:x".into());
        let a = db.create_issue(&new("Add the MCP server"), &agent).unwrap();
        let b = db.create_issue(&new("Inbox view for asks"), &agent).unwrap();
        assert_eq!((a.key.as_str(), b.key.as_str()), ("NIT-1", "NIT-2"));
        let err = db.create_issue(&new("Add an MCP server"), &agent).unwrap_err();
        assert!(err.to_string().contains("NIT-1"), "{err}");
        let mut forced = new("Add an MCP server");
        forced.allow_similar = true;
        assert_eq!(db.create_issue(&forced, &agent).unwrap().key, "NIT-3");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enforces_move_rules() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:x".into());
        db.create_issue(&new("One"), &agent).unwrap();
        assert!(db.move_issue("NIT-1", "done", None, None, &agent).is_err());
        assert!(db.move_issue("NIT-1", "canceled", None, None, &agent).is_err());
        assert!(db.move_issue("NIT-1", "approved", None, None, &agent).is_err());
        assert_eq!(db.move_issue("nit-1", "approved", None, None, &Actor::Human).unwrap().state, "Approved");
        let done = db.move_issue("NIT-1", "Done", Some("cargo test passed"), None, &agent).unwrap();
        assert_eq!((done.state.as_str(), done.evidence.as_str()), ("Done", "cargo test passed"));
        assert_eq!(db.timeline("NIT-1").unwrap().len(), 3);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn next_task_claims_most_urgent_once() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        let mut low = new("Low thing");
        low.priority = 4;
        db.create_issue(&low, &h).unwrap();
        let mut urgent = new("Urgent thing");
        urgent.priority = 1;
        db.create_issue(&urgent, &h).unwrap();
        let first = db.next_task("NIT", "claude:a").unwrap().unwrap();
        assert_eq!(first.key, "NIT-2");
        assert_eq!(first.claimed_by.as_deref(), Some("claude:a"));
        assert_eq!(db.next_task("NIT", "codex:b").unwrap().unwrap().key, "NIT-1");
        assert!(db.next_task("NIT", "cursor:c").unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn asks_are_answered_by_humans_only() {
        let (db, dir) = db();
        let agent = Actor::Agent("claude:x".into());
        let ask = db.create_ask("Ship it?", &["yes".into(), "no".into()], None, None, &agent).unwrap();
        assert!(db.answer_ask(ask.id, "yes", &agent).is_err());
        assert_eq!(db.answer_ask(ask.id, "yes", &Actor::Human).unwrap().answer.as_deref(), Some("yes"));
        assert!(db.open_asks().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn helpers() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_792_125_417), "2026-10-16T04:36:57Z");
        assert_eq!(branch_name("claude", "NIT-12", "Fix the sidebar's width!"), "claude/nit-12-fix-the-sidebar-s-width");
        assert_eq!(parse_state("needs-andrew").unwrap().0, "Needs You");
        assert!(title_similarity("Add the MCP server", "add mcp server now") >= 0.6);
    }
}
