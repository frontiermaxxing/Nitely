//! Projects and the native backlog. The rules are the ones `scripts/work/linear.mjs` enforces in the
//! Aera workflow, so agents file and move issues the same way they already do: a kind and a source
//! on every issue, evidence to file or finish one, a reason to cancel, a duplicate check before
//! filing, and Approved reserved for a human.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use crate::db::{now, Db};
use crate::track::{track_of, Track};

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
    /// The link to name the issue by: `nitely://issue/KEY` for native issues, the source's address
    /// for mirrored ones.
    pub url: String,
    /// Where a native issue came from, e.g. its Linear address after an import; empty otherwise.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub external_url: String,
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
    /// Open issues that must close before this one can start. `next_task` skips it until then.
    pub blocked_by: Vec<String>,
    /// Open issues waiting on this one.
    pub blocks: Vec<String>,
    /// The priority it is worked at: its own, raised to that of the most urgent open issue it
    /// blocks, directly or through other blockers. Same scale as `priority`.
    pub urgency: i64,
    /// Where it is on the lifecycle track (`track.rs`). Open issues and single lookups use their
    /// timeline; closed issues in a list only their state.
    pub track: Track,
}

impl IssueRecord {
    pub fn is_open(&self) -> bool {
        !matches!(self.state_type.as_str(), "completed" | "canceled")
    }

    pub fn needs_human(&self) -> bool {
        NEEDS_HUMAN.contains(&self.state.as_str())
    }
}

/// An issue on the other end of a blocked-by relation, open or closed.
#[derive(Debug, Clone, Serialize)]
pub struct LinkedIssue {
    pub key: String,
    pub title: String,
    pub state: String,
    pub open: bool,
}

/// One issue with everything the issue panel shows.
#[derive(Debug, Clone, Serialize)]
pub struct IssueDetail {
    pub issue: IssueRecord,
    pub relations: Relations,
    pub timeline: Vec<TimelineEntry>,
    pub prs: Vec<crate::db::PrRecord>,
    pub reviews: Vec<crate::submit::ReviewRecord>,
}

/// Both directions of an issue's blocked-by relations, closed issues included.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Relations {
    pub blocked_by: Vec<LinkedIssue>,
    pub blocks: Vec<LinkedIssue>,
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
    /// Issues that must close before this one can start.
    pub blocked_by: Vec<String>,
}

const ISSUE_COLUMNS: &str = "key, origin, project, title, body, url, state, state_type, kind, source,
    priority, labels, evidence, claimed_by, created_at, updated_at";

/// The scheme the app registers, so a link to an issue opens it in Nitely.
pub const LINK_SCHEME: &str = "nitely";

/// `nitely://issue/NIT-14`: the link that opens a native issue in the app. Agents put it wherever
/// they name an issue (replies, commits, pull requests, notes), as Linear links are used.
pub fn issue_link(key: &str) -> String {
    format!("{LINK_SCHEME}://issue/{}", key.to_uppercase())
}

/// The issue key a `nitely://issue/KEY` link names, if it is one ("nitely://issue/nit-14/" works too).
pub fn key_from_link(link: &str) -> Option<String> {
    let rest = link.trim().strip_prefix(LINK_SCHEME)?.strip_prefix("://")?;
    let key = rest.strip_prefix("issue/")?.trim_end_matches('/');
    let (project, number) = key.split_once('-')?;
    let ok = !project.is_empty()
        && project.starts_with(|c: char| c.is_ascii_alphabetic())
        && project.chars().all(|c| c.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit());
    ok.then(|| key.to_uppercase())
}

fn issue_from_row(r: &Row) -> rusqlite::Result<IssueRecord> {
    let origin: String = r.get(1)?;
    let key: String = r.get(0)?;
    let stored: String = r.get(5)?;
    // A native issue's link is always the app's own. What is stored for it is where it came from
    // (an imported Linear issue's address), shown beside it but never handed out as its link.
    let (url, external_url) = if origin == "nitely" { (issue_link(&key), stored) } else { (stored, String::new()) };
    Ok(IssueRecord {
        key,
        origin,
        project: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        url,
        external_url,
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
        ..Default::default()
    })
}

/// 0 (none) sorts after 4 (low).
fn rank(priority: i64) -> i64 {
    if priority == 0 { 5 } else { priority }
}

/// Every blocked-by relation, with whether each issue is open and its priority. Both ends are
/// native issues, which are never deleted.
struct Deps {
    /// (blocked issue, blocker)
    edges: Vec<(String, String)>,
    open: HashMap<String, bool>,
    priority: HashMap<String, i64>,
}

impl Deps {
    fn load(conn: &rusqlite::Connection) -> Result<Deps> {
        let mut stmt = conn.prepare(
            "SELECT r.issue_key, r.blocker_key, a.state_type, a.priority, b.state_type, b.priority
             FROM issue_blocker r
             JOIN issue a ON a.key = r.issue_key
             JOIN issue b ON b.key = r.blocker_key
             ORDER BY r.issue_key, r.blocker_key",
        )?;
        let mut deps = Deps { edges: Vec::new(), open: HashMap::new(), priority: HashMap::new() };
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?))
        })?;
        for row in rows {
            let (issue, blocker, issue_type, issue_priority, blocker_type, blocker_priority) = row?;
            let open = |t: &str| !matches!(t, "completed" | "canceled");
            deps.open.insert(issue.clone(), open(&issue_type));
            deps.open.insert(blocker.clone(), open(&blocker_type));
            deps.priority.insert(issue.clone(), issue_priority);
            deps.priority.insert(blocker.clone(), blocker_priority);
            deps.edges.push((issue, blocker));
        }
        Ok(deps)
    }

    fn is_open(&self, key: &str) -> bool {
        self.open.get(key).copied().unwrap_or(false)
    }

    fn blocked_by(&self, key: &str) -> Vec<String> {
        self.edges.iter().filter(|(i, b)| i == key && self.is_open(b)).map(|(_, b)| b.clone()).collect()
    }

    fn blocks(&self, key: &str) -> Vec<String> {
        self.edges.iter().filter(|(i, b)| b == key && self.is_open(i)).map(|(i, _)| i.clone()).collect()
    }

    /// `priority`, raised to the most urgent open issue `key` blocks, directly or transitively.
    fn urgency(&self, key: &str, priority: i64) -> i64 {
        let mut best = rank(priority);
        let mut seen = HashSet::from([key.to_string()]);
        let mut todo = vec![key.to_string()];
        while let Some(k) = todo.pop() {
            for blocked in self.blocks(&k) {
                if seen.insert(blocked.clone()) {
                    best = best.min(rank(self.priority.get(&blocked).copied().unwrap_or(0)));
                    todo.push(blocked);
                }
            }
        }
        if best == 5 { 0 } else { best }
    }

    /// Whether `from` waits on `to`, directly or through other blockers (open or closed).
    fn waits_on(&self, from: &str, to: &str) -> bool {
        let mut seen = HashSet::new();
        let mut todo = vec![from.to_string()];
        while let Some(k) = todo.pop() {
            for (i, b) in &self.edges {
                if *i == k {
                    if b == to {
                        return true;
                    }
                    if seen.insert(b.clone()) {
                        todo.push(b.clone());
                    }
                }
            }
        }
        false
    }

    fn fill(&self, issue: &mut IssueRecord) {
        issue.blocked_by = self.blocked_by(&issue.key);
        issue.blocks = self.blocks(&issue.key);
        issue.urgency = self.urgency(&issue.key, issue.priority);
    }
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
        let key = crate::projects::valid_key(key)?;
        let taken: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM project WHERE key = ?1)
                 OR EXISTS (SELECT 1 FROM issue WHERE key LIKE ?1 || '-%')",
            [&key],
            |r| r.get(0),
        )?;
        if taken {
            bail!("{key} is already a project or an issue prefix");
        }
        // An earlier key of a project keeps its old links; a new project must not take them over.
        if let Some(project) = self.alias_of(&format!("{key}-1"))? {
            bail!("{key} was an earlier key of {}, and its old links still lead there", project.trim_end_matches("-1"));
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

    /// A project by its key, or by a key it had before (so an old `NITELY_PROJECT`, config entry
    /// or machine token still reaches it).
    pub fn project(&self, key: &str) -> Result<Option<Project>> {
        if let Some(p) = self.project_exact(key)? {
            return Ok(Some(p));
        }
        match self.project_aliases()?.get(&key.trim().to_uppercase()) {
            Some(current) => self.project_exact(current),
            None => Ok(None),
        }
    }

    /// A project by its current key only.
    pub fn project_exact(&self, key: &str) -> Result<Option<Project>> {
        Ok(self.projects()?.into_iter().find(|p| p.key.eq_ignore_ascii_case(key.trim())))
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

        let mut blockers: Vec<String> = Vec::new();
        for b in &new.blocked_by {
            let blocker = self.issue(b.trim())?.with_context(|| format!("no issue {b} to be blocked by"))?;
            if blocker.origin != "nitely" {
                bail!("{} is mirrored from {}, which drops it when it closes; only native issues can block", blocker.key, blocker.origin);
            }
            if !blockers.contains(&blocker.key) {
                blockers.push(blocker.key);
            }
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
        for blocker in &blockers {
            tx.execute(
                "INSERT INTO issue_blocker (issue_key, blocker_key, created_at) VALUES (?1, ?2, ?3)",
                params![key, blocker, ts],
            )?;
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'blocker_added', ?4)",
                params![ts, actor.id(), key, format!("Blocked by {blocker}")],
            )?;
        }
        tx.commit()?;
        self.issue(&key)?.context("issue vanished after insert")
    }

    /// An issue by its key, or by a key from before its project changed its key (`OLD-12`).
    pub fn issue(&self, key: &str) -> Result<Option<IssueRecord>> {
        let find = |key: &str| {
            self.conn
                .query_row(&format!("SELECT {ISSUE_COLUMNS} FROM issue WHERE key = ?1 COLLATE NOCASE"), [key], issue_from_row)
                .optional()
        };
        let issue = match find(key)? {
            Some(i) => Some(i),
            None => match self.alias_of(key)? {
                Some(current) => find(&current)?,
                None => None,
            },
        };
        Ok(match issue {
            Some(mut i) => {
                Deps::load(&self.conn)?.fill(&mut i);
                let changes = self.change_reviews()?.get(&i.key.to_uppercase()).copied().unwrap_or(0);
                i.track = track_of(&i, &self.timeline(&i.key)?, changes);
                Some(i)
            }
            None => None,
        })
    }

    /// Every issue (native and mirrored), most urgent first; `project` narrows to one project's
    /// native issues.
    pub fn list_issues(&self, project: Option<&str>, include_closed: bool) -> Result<Vec<IssueRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ISSUE_COLUMNS} FROM issue
             WHERE (?1 IS NULL OR project = ?1 COLLATE NOCASE)
               AND (?2 OR state_type NOT IN ('completed', 'canceled'))
             ORDER BY CASE priority WHEN 0 THEN 5 ELSE priority END, number, key"
        ))?;
        let rows = stmt.query_map(params![project, include_closed], issue_from_row)?;
        let mut issues: Vec<IssueRecord> = rows.collect::<rusqlite::Result<_>>()?;
        let deps = Deps::load(&self.conn)?;
        for i in &mut issues {
            deps.fill(i);
        }
        self.fill_tracks(&mut issues)?;
        // Stable, so issues of equal urgency keep their priority and number order.
        issues.sort_by_key(|i| rank(i.urgency));
        Ok(issues)
    }

    /// Records that `key` cannot start before `blocker` closes.
    pub fn add_blocker(&mut self, key: &str, blocker: &str, actor: &Actor) -> Result<()> {
        self.change_blockers(key, &[blocker.to_string()], &[], actor)
    }

    pub fn remove_blocker(&mut self, key: &str, blocker: &str, actor: &Actor) -> Result<()> {
        self.change_blockers(key, &[], &[blocker.to_string()], actor)
    }

    /// Adds and removes blockers of `key` together: all of them or, on any error, none. Refuses a
    /// blocker that is missing, mirrored, `key` itself, or already waiting on `key`, and removing a
    /// blocker `key` does not have. Only native issues block: the Linear mirror holds open issues
    /// only and drops one when it closes, so it cannot say when a blocker finished.
    pub fn change_blockers(&mut self, key: &str, add: &[String], remove: &[String], actor: &Actor) -> Result<()> {
        let ts = now();
        // Keys from before a project changed its key name the same issues.
        let aliases = self.project_aliases()?;
        let current = |k: &str| crate::projects::current_key(&aliases, k.trim());
        let key = &current(key);
        let add: Vec<String> = add.iter().map(|k| current(k)).collect();
        let remove: Vec<String> = remove.iter().map(|k| current(k)).collect();
        // Immediate: the cycle check and the insert see the same relations even with agents racing.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let find = |k: &str| -> Result<(String, String)> {
            tx.query_row("SELECT key, origin FROM issue WHERE key = ?1 COLLATE NOCASE", [k.trim()], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .with_context(|| format!("no issue {k}"))
        };
        let (issue, origin) = find(key)?;
        for b in &remove {
            let b = b.trim().to_uppercase();
            let removed = tx.execute(
                "DELETE FROM issue_blocker WHERE issue_key = ?1 AND blocker_key = ?2 COLLATE NOCASE",
                params![issue, b],
            )?;
            if removed == 0 {
                bail!("{issue} is not blocked by {b}");
            }
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'blocker_removed', ?4)",
                params![ts, actor.id(), issue, format!("No longer blocked by {b}")],
            )?;
        }
        if !add.is_empty() && origin != "nitely" {
            bail!("{issue} is mirrored from {origin}; only native issues take blockers");
        }
        for b in &add {
            let (blocker, blocker_origin) = find(b)?;
            if blocker_origin != "nitely" {
                bail!("{blocker} is mirrored from {blocker_origin}, which drops it when it closes; only native issues can block");
            }
            if blocker == issue {
                bail!("{issue} cannot block itself");
            }
            if Deps::load(&tx)?.waits_on(&blocker, &issue) {
                bail!("{blocker} already waits on {issue}, so {issue} cannot also wait on it");
            }
            let added = tx.execute(
                "INSERT OR IGNORE INTO issue_blocker (issue_key, blocker_key, created_at) VALUES (?1, ?2, ?3)",
                params![issue, blocker, ts],
            )?;
            if added > 0 {
                tx.execute(
                    "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'blocker_added', ?4)",
                    params![ts, actor.id(), issue, format!("Blocked by {blocker}")],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Both directions of `key`'s blocked-by relations, closed issues included.
    pub fn relations(&self, key: &str) -> Result<Relations> {
        let link = |sql: &str| -> Result<Vec<LinkedIssue>> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([key], |r| {
                let state_type: String = r.get(3)?;
                Ok(LinkedIssue {
                    key: r.get(0)?,
                    title: r.get(1)?,
                    state: r.get(2)?,
                    open: !matches!(state_type.as_str(), "completed" | "canceled"),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        Ok(Relations {
            blocked_by: link(
                "SELECT i.key, i.title, i.state, i.state_type FROM issue_blocker r JOIN issue i ON i.key = r.blocker_key
                 WHERE r.issue_key = ?1 COLLATE NOCASE ORDER BY i.number, i.key",
            )?,
            blocks: link(
                "SELECT i.key, i.title, i.state, i.state_type FROM issue_blocker r JOIN issue i ON i.key = r.issue_key
                 WHERE r.blocker_key = ?1 COLLATE NOCASE ORDER BY i.number, i.key",
            )?,
        })
    }

    /// Kept for the snapshot and older callers: every stored issue, open or not.
    pub fn issues(&self) -> Result<Vec<IssueRecord>> {
        self.list_issues(None, true)
    }

    /// Refuses the moves `move_issue` refuses, without moving anything.
    pub fn check_move(&self, key: &str, to: &str, evidence: Option<&str>, reason: Option<&str>, actor: &Actor) -> Result<()> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.origin != "nitely" {
            bail!("{} is mirrored from {}; change it there", issue.key, issue.origin);
        }
        let (state, _) = parse_state(to)?;
        if state == "Approved" && *actor != Actor::Human {
            bail!("only a human can approve; ask for approval with `ask` and move the issue to Needs You");
        }
        let given = |v: Option<&str>| v.is_some_and(|v| !v.trim().is_empty());
        if state == "Done" && !given(evidence) {
            bail!("moving to Done needs evidence: what ran and what it showed");
        }
        if matches!(state, "Canceled" | "Duplicate") && !given(reason) {
            bail!("moving to {state} needs a reason");
        }
        Ok(())
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
        self.check_move(key, to, evidence, reason, actor)?;
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        let (state, state_type) = parse_state(to)?;
        let evidence = evidence.map(str::trim).filter(|e| !e.is_empty());
        let reason = reason.map(str::trim).filter(|r| !r.is_empty());

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
        // Closing a blocker readies the issues that waited only on it.
        if issue.is_open() && matches!(state_type, "completed" | "canceled") {
            let deps = Deps::load(&tx)?;
            for waiting in issue.blocks.iter().filter(|k| deps.blocked_by(k).is_empty()) {
                tx.execute(
                    "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'unblocked', ?4)",
                    params![ts, actor.id(), waiting, format!("Ready: {} is {state}", issue.key)],
                )?;
            }
        }
        tx.commit()?;
        self.issue(&issue.key)?.context("issue vanished")
    }

    /// Claims the most urgent Todo issue of a project for `agent`, atomically: two agents asking at
    /// once get different issues. A claim untouched for `CLAIM_TTL_SECS` counts as abandoned. An
    /// issue with an open blocker is skipped, and a blocker is as urgent as what it blocks.
    pub fn next_task(&mut self, project: &str, agent: &str) -> Result<Option<IssueRecord>> {
        let ts = now();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates: Vec<(String, i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT key, priority, number FROM issue
                 WHERE project = ?1 COLLATE NOCASE AND origin = 'nitely'
                   AND (state = 'Todo' OR (state = 'In Progress' AND claimed_at < ?2))",
            )?;
            let rows = stmt.query_map(params![project, ts - CLAIM_TTL_SECS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let deps = Deps::load(&tx)?;
        let key = candidates
            .into_iter()
            .filter(|(k, _, _)| deps.blocked_by(k).is_empty())
            .min_by_key(|(k, priority, number)| (rank(deps.urgency(k, *priority)), *number))
            .map(|(k, _, _)| k);
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

    /// Fills each issue's track: open issues from their timelines, read in one query; closed ones
    /// from their state, which is all a list shows of them.
    fn fill_tracks(&self, issues: &mut [IssueRecord]) -> Result<()> {
        let mut timelines: HashMap<String, Vec<TimelineEntry>> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT e.issue_key, e.ts, e.actor, e.type, e.summary FROM event e
             JOIN issue i ON i.key = e.issue_key COLLATE NOCASE
             WHERE i.state_type NOT IN ('completed', 'canceled')
             ORDER BY e.ts, e.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, TimelineEntry { ts: r.get(1)?, actor: r.get(2)?, kind: r.get(3)?, summary: r.get(4)? }))
        })?;
        for row in rows {
            let (key, entry) = row?;
            timelines.entry(key.to_uppercase()).or_default().push(entry);
        }
        let changes = self.change_reviews()?;
        for i in issues.iter_mut() {
            let key = i.key.to_uppercase();
            let timeline = if i.is_open() { timelines.get(&key).map(Vec::as_slice).unwrap_or(&[]) } else { &[] };
            i.track = track_of(i, timeline, changes.get(&key).copied().unwrap_or(0));
        }
        Ok(())
    }

    /// Review rounds that asked for changes, per issue key (uppercase).
    fn change_reviews(&self) -> Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare("SELECT issue_key, COUNT(*) FROM review WHERE verdict = 'changes' GROUP BY issue_key")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?.to_uppercase(), r.get::<_, i64>(1)?)))?;
        let mut counts = HashMap::new();
        for row in rows {
            let (key, n) = row?;
            *counts.entry(key).or_insert(0) += n;
        }
        Ok(counts)
    }

    /// The issue panel's read: the issue, its relations, timeline, pull requests and reviews.
    pub fn issue_detail(&self, key: &str) -> Result<Option<IssueDetail>> {
        let Some(issue) = self.issue(key)? else { return Ok(None) };
        Ok(Some(IssueDetail {
            relations: self.relations(&issue.key)?,
            timeline: self.timeline(&issue.key)?,
            prs: self.prs_for_issue(&issue.key)?,
            reviews: self.reviews(&issue.key)?,
            issue,
        }))
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
        // A project's old key belongs to it: its links open the native issues under the new key,
        // so a tracker that still has issues with that prefix does not mirror them back in.
        let aliases = self.project_aliases()?;
        let taken = |key: &str| key.split_once('-').is_some_and(|(p, _)| aliases.contains_key(&p.to_uppercase()));
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM issue WHERE origin = ?1", [origin])?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO issue (key, origin, title, url, state, state_type, labels, updated_at, priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for i in issues.iter().filter(|i| !taken(&i.key)) {
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
        // A counter as well as the time: Windows' clock can give two tests started together the
        // same time, and then the same database.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nitely-issues-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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
    fn blocked_issues_wait_and_blockers_inherit_urgency() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        let mut base = new("Visual foundation");
        base.priority = 3;
        db.create_issue(&base, &h).unwrap(); // NIT-1
        let mut view = new("Board view");
        view.priority = 1;
        view.blocked_by = vec!["nit-1".into()];
        db.create_issue(&view, &h).unwrap(); // NIT-2, waits on NIT-1
        let mut other = new("Unrelated cleanup");
        other.priority = 2;
        db.create_issue(&other, &h).unwrap(); // NIT-3

        let blocked = db.issue("NIT-2").unwrap().unwrap();
        assert_eq!(blocked.blocked_by, vec!["NIT-1"]);
        let blocker = db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((blocker.blocks.clone(), blocker.priority, blocker.urgency), (vec!["NIT-2".to_string()], 3, 1));
        let order: Vec<String> = db.list_issues(Some("NIT"), false).unwrap().into_iter().map(|i| i.key).collect();
        assert_eq!(order, ["NIT-2", "NIT-1", "NIT-3"]);

        // NIT-1 is as urgent as NIT-2, which it blocks, so it goes before NIT-3; NIT-2 is skipped.
        assert_eq!(db.next_task("NIT", "claude:a").unwrap().unwrap().key, "NIT-1");
        assert_eq!(db.next_task("NIT", "claude:b").unwrap().unwrap().key, "NIT-3");
        assert!(db.next_task("NIT", "claude:c").unwrap().is_none());

        db.move_issue("NIT-1", "Done", Some("landed"), None, &h).unwrap();
        let ready = db.issue("NIT-2").unwrap().unwrap();
        assert!(ready.blocked_by.is_empty());
        assert!(db.timeline("NIT-2").unwrap().iter().any(|e| e.kind == "unblocked"));
        assert_eq!(db.next_task("NIT", "claude:c").unwrap().unwrap().key, "NIT-2");
        // The relation stays on record after the blocker closes.
        assert!(!db.relations("NIT-2").unwrap().blocked_by[0].open);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn blockers_chain_and_refuse_cycles() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        for t in ["Foundation", "Board timeline", "Inbox redesign", "Review everything"] {
            db.create_issue(&new(t), &h).unwrap();
        }
        db.add_blocker("NIT-2", "NIT-1", &h).unwrap();
        db.add_blocker("NIT-3", "NIT-1", &h).unwrap();
        db.add_blocker("NIT-4", "NIT-2", &h).unwrap();
        db.add_blocker("NIT-4", "NIT-3", &h).unwrap();
        db.add_blocker("NIT-4", "NIT-3", &h).unwrap(); // again: no-op
        assert!(db.add_blocker("NIT-1", "NIT-4", &h).is_err(), "cycle through two levels");
        assert!(db.add_blocker("NIT-1", "NIT-1", &h).is_err());
        assert!(db.add_blocker("NIT-1", "NIT-99", &h).is_err());
        let mut late = new("Something filed late");
        late.blocked_by = vec!["NIT-99".into()];
        assert!(db.create_issue(&late, &h).is_err(), "a missing blocker refuses the filing");

        // Urgency flows down the chain: NIT-4 urgent makes NIT-1 urgent too.
        db.conn.execute("UPDATE issue SET priority = 1 WHERE key = 'NIT-4'", []).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().urgency, 1);
        assert_eq!(db.issue("NIT-4").unwrap().unwrap().blocked_by, vec!["NIT-2", "NIT-3"]);

        db.remove_blocker("NIT-4", "nit-2", &h).unwrap();
        assert!(db.remove_blocker("NIT-4", "NIT-2", &h).is_err());
        assert_eq!(db.issue("NIT-4").unwrap().unwrap().blocked_by, vec!["NIT-3"]);
        let rel = db.relations("NIT-1").unwrap();
        assert_eq!(rel.blocks.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), ["NIT-2", "NIT-3"]);

        // All or nothing: a bad key in the batch leaves the good one unadded.
        let batch = ["NIT-1".to_string(), "NIT-99".to_string()];
        assert!(db.change_blockers("NIT-4", &batch, &[], &h).is_err());
        assert_eq!(db.issue("NIT-4").unwrap().unwrap().blocked_by, vec!["NIT-3"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mirrored_issues_cannot_block() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        let linear = IssueRecord { key: "AERA-5".into(), title: "Upstream fix".into(), state: "Todo".into(), state_type: "unstarted".into(), ..Default::default() };
        db.replace_issues("linear", &[linear]).unwrap();
        let mut waits = new("Use the upstream fix");
        waits.blocked_by = vec!["AERA-5".into()];
        assert!(db.create_issue(&waits, &h).is_err());
        db.create_issue(&new("Use the upstream fix"), &h).unwrap();
        assert!(db.add_blocker("NIT-1", "AERA-5", &h).is_err());
        assert!(db.add_blocker("AERA-5", "NIT-1", &h).is_err());
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
    fn native_issues_carry_their_app_link() {
        assert_eq!(issue_link("nit-14"), "nitely://issue/NIT-14");
        assert_eq!(key_from_link("nitely://issue/NIT-14"), Some("NIT-14".into()));
        assert_eq!(key_from_link("nitely://issue/nit-14/"), Some("NIT-14".into()));
        assert_eq!(key_from_link("nitely://issue/N2-7"), Some("N2-7".into()));
        assert_eq!(key_from_link("nitely://issue/NIT-"), None);
        assert_eq!(key_from_link("nitely://issue/14-NIT"), None);
        assert_eq!(key_from_link("https://issue/NIT-14"), None);
        assert_eq!(key_from_link("nitely://project/NIT"), None);
        let (mut db, _dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let issue = db.create_issue(&new("Links that open the app"), &Actor::Agent("claude:x".into())).unwrap();
        assert_eq!(issue.url, issue_link(&issue.key));
        assert_eq!(db.list_issues(Some("NIT"), false).unwrap()[0].url, issue.url);

        // An imported issue keeps where it came from, but its link is still the app's.
        let linear = "https://linear.app/x/issue/NIT-1/links";
        db.conn.execute("UPDATE issue SET url = ?2 WHERE key = ?1", rusqlite::params![issue.key, linear]).unwrap();
        let imported = db.issue(&issue.key).unwrap().unwrap();
        assert_eq!((imported.url.as_str(), imported.external_url.as_str()), (issue_link(&issue.key).as_str(), linear));
        assert_eq!(db.list_issues(Some("NIT"), false).unwrap()[0].url, issue_link(&issue.key));
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
