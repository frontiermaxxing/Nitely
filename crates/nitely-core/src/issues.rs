//! Projects and the native backlog, with the rules every agent files and moves issues by: a kind
//! and a source on every issue, evidence to file or finish one, a reason to cancel, a duplicate
//! check before filing, and Approved reserved for a human.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use crate::db::{now, Db};
use crate::similar;
use crate::track::{track_of, Track};

pub const KINDS: &[&str] = &["bug", "feature", "improvement", "idea", "live-check"];
pub const SOURCES: &[&str] = &["claude", "codex", "cursor", "human"];
/// Who does an issue's work: agents, or only the human (buy hardware, sign a contract, rotate a
/// credential in a console). `next_task` never hands out the human's own work.
pub const OWNERS: &[&str] = &["agent", "human"];

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

/// The state that means the next step is the human's.
pub const NEEDS_HUMAN: &str = "Needs You";

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
    /// `agent`, or `human` for work only the human can do (see `OWNERS`).
    pub owner: String,
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
        self.state == NEEDS_HUMAN
    }

    pub fn is_humans(&self) -> bool {
        self.owner == "human"
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
    /// The playbook change the issue carries, when an agent proposed one.
    pub proposal: Option<crate::proposals::Proposal>,
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
    /// Defaults to Todo, so `next_task` can hand it out; Needs You for the human's own work.
    pub state: Option<String>,
    /// `agent` (the default) or `human`.
    pub owner: Option<String>,
    /// Issues that must close before this one can start.
    pub blocked_by: Vec<String>,
    /// Areas and platforms, e.g. `ios`, `billing`; normalized by `normalize_label`.
    pub labels: Vec<String>,
}

/// One issue of a `file_issues` batch: a new issue, and the ref the batch's other items name it by
/// in their `blocked_by`.
#[derive(Debug, Clone, Default)]
pub struct BatchItem {
    pub local_ref: String,
    pub issue: NewIssue,
}

/// What a batch filed: each ref's key (the new issue, or the one it duplicates), the issues filed in
/// the order they were, and the items skipped as likely duplicates.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BatchFiled {
    pub keys: std::collections::BTreeMap<String, String>,
    pub filed: Vec<Filed>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Filed {
    #[serde(rename = "ref")]
    pub local_ref: String,
    pub key: String,
    pub title: String,
    pub url: String,
    #[serde(skip)]
    pub project: String,
    /// Its place in the batch.
    #[serde(skip)]
    pub item: usize,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    #[serde(rename = "ref")]
    pub local_ref: String,
    pub title: String,
    /// The open issues (or the earlier item) it likely duplicates.
    pub duplicate_of: Vec<String>,
    /// What the items that named its ref are blocked by instead.
    pub stands_for: String,
    /// Why it looked like `stands_for`.
    pub why: String,
}

/// An issue a new one likely duplicates, and why it looked similar.
struct Duplicate {
    issue: IssueRecord,
    why: String,
}

impl Duplicate {
    /// `NIT-3 Inbox count is wrong (closed: Done): title shares "inbox", "count"`.
    fn line(&self) -> String {
        let closed = if self.issue.is_open() { String::new() } else { format!(" (closed: {})", self.issue.state) };
        format!("{} {}{closed}: {}", self.issue.key, self.issue.title, self.why)
    }
}

/// A new issue that passed `check_new`, ready to insert.
struct CheckedIssue {
    project: String,
    title: String,
    body: String,
    evidence: String,
    kind: String,
    source: String,
    priority: i64,
    owner: String,
    labels: Vec<String>,
    state: &'static str,
    state_type: &'static str,
}

impl CheckedIssue {
    fn text(&self) -> similar::Text<'_> {
        similar::Text { title: &self.title, body: &self.body, evidence: &self.evidence }
    }
}

/// Inserts a checked issue and its blockers, numbering it in its project; returns its key.
fn insert_issue(tx: &rusqlite::Connection, c: &CheckedIssue, blockers: &[String], ts: i64, actor: &Actor) -> Result<String> {
    let number: i64 = tx.query_row(
        "UPDATE project SET next_number = next_number + 1 WHERE key = ?1 RETURNING next_number - 1",
        [&c.project],
        |r| r.get(0),
    )?;
    let key = format!("{}-{number}", c.project);
    tx.execute(
        "INSERT INTO issue (key, origin, project, number, title, body, state, state_type, kind, source,
           priority, evidence, created_at, updated_at, owner, labels)
         VALUES (?1, 'nitely', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?14)",
        params![
            key,
            c.project,
            number,
            c.title,
            c.body,
            c.state,
            c.state_type,
            c.kind,
            c.source,
            c.priority,
            c.evidence,
            iso(ts),
            c.owner,
            serde_json::to_string(&c.labels)?
        ],
    )?;
    let filed = if c.owner == "human" { format!("Filed as {}, the human's own work", c.state) } else { format!("Filed as {}", c.state) };
    tx.execute(
        "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'issue_created', ?4)",
        params![ts, actor.id(), key, filed],
    )?;
    for blocker in blockers {
        tx.execute("INSERT INTO issue_blocker (issue_key, blocker_key, created_at) VALUES (?1, ?2, ?3)", params![key, blocker, ts])?;
        tx.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'blocker_added', ?4)",
            params![ts, actor.id(), key, format!("Blocked by {blocker}")],
        )?;
    }
    Ok(key)
}

const ISSUE_COLUMNS: &str = "key, origin, project, title, body, url, state, state_type, kind, source,
    priority, labels, evidence, claimed_by, created_at, updated_at, owner";

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
    // Native labels read in their normal form (an import may have stored "Bug"); a mirror's stay as
    // its tracker spells them.
    let labels: String = r.get(11)?;
    let labels = if origin == "nitely" { stored_labels(&labels) } else { serde_json::from_str(&labels).unwrap_or_default() };
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
        labels,
        evidence: r.get(12)?,
        claimed_by: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        owner: r.get(16)?,
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

/// Accepts "in progress", "in-progress" and "IN_PROGRESS".
pub fn parse_state(input: &str) -> Result<(&'static str, &'static str)> {
    let norm = |s: &str| s.to_lowercase().replace(['-', '_'], " ");
    let want = norm(input.trim());
    STATES
        .iter()
        .find(|(name, _)| norm(name) == want)
        .copied()
        .with_context(|| {
            let names: Vec<&str> = STATES.iter().map(|(n, _)| *n).collect();
            format!("unknown state {input:?}; one of: {}", names.join(", "))
        })
}

/// A label as Nitely keeps it: lowercase, words joined by `-`, so "Roku TV", "roku_tv" and
/// "roku-tv" are one label. None when nothing usable is left.
pub fn normalize_label(label: &str) -> Option<String> {
    let mut out = String::new();
    // Lowercased first, so a letter that lowercases to two characters ("İ" to "i" and a combining
    // dot) is normalized once and for all: normalizing a normal label changes nothing.
    for c in label.trim().to_lowercase().chars() {
        match c {
            // Combining diacritical marks, which lowercasing can produce: dropped, not a dash.
            '\u{300}'..='\u{36f}' => {}
            c if c.is_alphanumeric() => out.push(c),
            '.' | '+' | ':' | '/' => out.push(c),
            _ if !out.ends_with('-') => out.push('-'),
            _ => {}
        }
    }
    let out = out.trim_matches('-').to_string();
    (!out.is_empty() && out.chars().count() <= 40).then_some(out)
}

/// An issue's stored labels in their normal form, each once. One that has no normal form (an
/// import's label over 40 characters) is kept as it is, so no edit drops it.
fn stored_labels(json: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in serde_json::from_str::<Vec<String>>(json).unwrap_or_default() {
        let n = normalize_label(&l).unwrap_or(l);
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// Normalized labels, each once, in the order given; refuses one with nothing usable in it.
pub fn normalize_labels(labels: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        let n = normalize_label(l).with_context(|| format!("\"{l}\" is not a usable label: give a short word such as ios or billing"))?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
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

/// `claude/nit-12-fix-sidebar`: the default branch name for an issue.
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
        let checked = self.check_new(new)?;
        let blockers = self.native_blockers(&new.blocked_by)?;
        if !new.allow_similar {
            let similar = self.likely_duplicates(&checked.project, new)?;
            if !similar.is_empty() {
                let listed: Vec<String> = similar.iter().map(Duplicate::line).collect();
                bail!("possible duplicates (file anyway with allow_similar if it is different work):\n  {}", listed.join("\n  "));
            }
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = insert_issue(&tx, &checked, &blockers, now(), actor)?;
        tx.commit()?;
        self.issue(&key)?.context("issue vanished after insert")
    }

    /// Files a batch of issues in one transaction, in dependency order: an item's `blocked_by` may
    /// name another item's ref as well as an existing issue. Unknown refs, cycles and invalid items
    /// refuse the whole batch before anything is filed. An item that looks like an open issue (or
    /// an earlier item of the batch) is skipped unless it allows similar; items that named it are
    /// blocked by the issue it duplicates instead.
    pub fn create_issues(&mut self, items: &[BatchItem], actor: &Actor) -> Result<BatchFiled> {
        if items.is_empty() {
            bail!("give at least one issue");
        }
        let refs: Vec<&str> = items.iter().map(|i| i.local_ref.trim()).collect();
        for (n, r) in refs.iter().enumerate() {
            if r.is_empty() {
                bail!("item {} has no ref; give every item a ref the others can name in blocked_by", n + 1);
            }
            if refs[..n].contains(r) {
                bail!("two items have the ref {r}");
            }
        }
        let index = |name: &str| refs.iter().position(|r| *r == name.trim());

        // Check every item, and split its blockers into refs of this batch and existing issues.
        let mut checked = Vec::with_capacity(items.len());
        let mut local: Vec<Vec<usize>> = Vec::with_capacity(items.len());
        let mut existing: Vec<Vec<String>> = Vec::with_capacity(items.len());
        for (n, item) in items.iter().enumerate() {
            let r = refs[n];
            checked.push(self.check_new(&item.issue).with_context(|| format!("item {r}"))?);
            let (mut mine, mut theirs) = (Vec::new(), Vec::new());
            for b in &item.issue.blocked_by {
                match index(b) {
                    Some(i) if i == n => bail!("item {r} cannot block itself"),
                    Some(i) if !mine.contains(&i) => mine.push(i),
                    Some(_) => {}
                    None => theirs.push(b.clone()),
                }
            }
            let theirs = self
                .native_blockers(&theirs)
                .with_context(|| format!("item {r}: blocked_by names neither a ref of this batch nor an existing issue"))?;
            local.push(mine);
            existing.push(theirs);
        }

        // Dependency order (Kahn's), keeping the given order among items that are ready together.
        let mut order: Vec<usize> = Vec::with_capacity(items.len());
        let mut placed = vec![false; items.len()];
        while order.len() < items.len() {
            match (0..items.len()).find(|&i| !placed[i] && local[i].iter().all(|&b| placed[b])) {
                Some(i) => {
                    placed[i] = true;
                    order.push(i);
                }
                None => {
                    let stuck: Vec<&str> = (0..items.len()).filter(|&i| !placed[i]).map(|i| refs[i]).collect();
                    bail!("the blockers of {} wait on each other in a cycle; nothing was filed", stuck.join(", "));
                }
            }
        }

        // Duplicates of open issues are found before the transaction; of earlier items, as it goes.
        let mut duplicates: Vec<Vec<Duplicate>> = Vec::with_capacity(items.len());
        for (item, c) in items.iter().zip(&checked) {
            duplicates.push(if item.issue.allow_similar { Vec::new() } else { self.likely_duplicates(&c.project, &item.issue)? });
        }

        let ts = now();
        let mut out = BatchFiled::default();
        // The key each ref stands for: the issue filed for it, or the one it duplicates.
        let mut key_of: Vec<Option<String>> = vec![None; items.len()];
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &i in &order {
            let (item, c) = (&items[i], &checked[i]);
            let earlier = if item.issue.allow_similar {
                None
            } else {
                out.filed.iter().find_map(|f| {
                    let e = &checked[f.item];
                    let likeness = similar::compare(&c.text(), &e.text());
                    (f.project == c.project && likeness.is_duplicate()).then(|| (f.key.clone(), format!("an earlier item of this batch: {}", likeness.why())))
                })
            };
            let (dup_of, why): (Vec<String>, String) = match earlier {
                Some((key, why)) => (vec![key], why),
                None => (duplicates[i].iter().map(|d| d.issue.key.clone()).collect(), duplicates[i].first().map(|d| d.why.clone()).unwrap_or_default()),
            };
            if let Some(first) = dup_of.first() {
                key_of[i] = Some(first.clone());
                out.skipped.push(Skipped { local_ref: refs[i].to_string(), title: c.title.clone(), stands_for: first.clone(), duplicate_of: dup_of, why });
                continue;
            }
            let mut blockers = existing[i].clone();
            for &b in &local[i] {
                let key = key_of[b].clone().context("a blocker was not filed before what it blocks")?;
                if !blockers.contains(&key) {
                    blockers.push(key);
                }
            }
            let key = insert_issue(&tx, c, &blockers, ts, actor)?;
            key_of[i] = Some(key.clone());
            out.filed.push(Filed {
                local_ref: refs[i].to_string(),
                url: issue_link(&key),
                key,
                title: c.title.clone(),
                project: c.project.clone(),
                item: i,
                blocked_by: blockers,
            });
        }
        tx.commit()?;
        out.keys = refs.iter().zip(key_of).filter_map(|(r, k)| Some((r.to_string(), k?))).collect();
        Ok(out)
    }

    /// Validates a new issue: everything but its blockers and the duplicate check.
    fn check_new(&self, new: &NewIssue) -> Result<CheckedIssue> {
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
        let owner = new.owner.as_deref().map(str::trim).filter(|o| !o.is_empty()).unwrap_or("agent");
        if !OWNERS.contains(&owner) {
            bail!("owner must be one of {}", OWNERS.join(", "));
        }
        let (state, state_type) = parse_state(new.state.as_deref().unwrap_or(if owner == "human" { "Needs You" } else { "Todo" }))?;
        if state == "Approved" {
            bail!("an issue cannot be filed as Approved");
        }
        Ok(CheckedIssue {
            project: project.key,
            title: title.to_string(),
            body: new.body.trim().to_string(),
            evidence: new.evidence.trim().to_string(),
            kind: new.kind.clone(),
            source: source.to_string(),
            priority: new.priority,
            owner: owner.to_string(),
            labels: normalize_labels(&new.labels)?,
            state,
            state_type,
        })
    }

    /// The keys of existing native issues, for blocked_by; refuses one that is missing or mirrored.
    fn native_blockers(&self, keys: &[String]) -> Result<Vec<String>> {
        let mut blockers: Vec<String> = Vec::new();
        for b in keys {
            let blocker = self.issue(b.trim())?.with_context(|| format!("no issue {b} to be blocked by"))?;
            if blocker.origin != "nitely" {
                bail!("{} is mirrored from {}, which drops it when it closes; only native issues can block", blocker.key, blocker.origin);
            }
            if !blockers.contains(&blocker.key) {
                blockers.push(blocker.key);
            }
        }
        Ok(blockers)
    }

    /// Issues of `project`, open or closed, that a new issue likely duplicates: the most alike first.
    /// Closed ones count, so work already done or dropped is not filed again.
    fn likely_duplicates(&self, project: &str, new: &NewIssue) -> Result<Vec<Duplicate>> {
        let text = similar::Text { title: new.title.trim(), body: new.body.trim(), evidence: new.evidence.trim() };
        let mut found: Vec<(f64, Duplicate)> = self
            .list_issues(Some(project), true)?
            .into_iter()
            .filter_map(|issue| {
                let likeness = similar::compare(&text, &similar::Text { title: &issue.title, body: &issue.body, evidence: &issue.evidence });
                likeness.is_duplicate().then(|| (likeness.title, Duplicate { why: likeness.why(), issue }))
            })
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(found.into_iter().map(|(_, d)| d).collect())
    }

    /// An issue by its key, or by a key from before its project changed its key (`OLD-12`).
    pub fn issue(&self, key: &str) -> Result<Option<IssueRecord>> {
        let find = |key: &str| {
            self.conn
                .query_row(&format!("SELECT {ISSUE_COLUMNS} FROM issue WHERE key = ?1 COLLATE NOCASE"), [key], issue_from_row)
                .optional()
        };
        // An old key resolves through its alias first: it belongs to the project that had it, even
        // if some other row (a mirror from an earlier sync) still carries it.
        let issue = match self.alias_of(key)? {
            Some(current) => find(&current)?.map_or_else(|| find(key), |i| Ok(Some(i)))?,
            None => find(key)?,
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
        // An issue carrying a playbook change: approving applies it, canceling rejects it.
        if let Some(moved) = self.before_move(&issue.key, state, reason, actor)? {
            return Ok(moved);
        }
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
        self.next_task_except(project, agent, &[])
    }

    /// `next_task`, passing over the issues in `except` (the crew's, for issues it cannot start now).
    pub fn next_task_except(&mut self, project: &str, agent: &str, except: &[String]) -> Result<Option<IssueRecord>> {
        let ts = now();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates: Vec<(String, i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT key, priority, number FROM issue
                 WHERE project = ?1 COLLATE NOCASE AND origin = 'nitely' AND owner != 'human'
                   AND (state = 'Todo' OR (state = 'In Progress' AND claimed_at < ?2))",
            )?;
            let rows = stmt.query_map(params![project, ts - CLAIM_TTL_SECS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let deps = Deps::load(&tx)?;
        let key = candidates
            .into_iter()
            .filter(|(k, _, _)| deps.blocked_by(k).is_empty() && !except.iter().any(|e| e.eq_ignore_ascii_case(k)))
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

    /// Every label used on a project's native issues, open or closed, with how many issues carry
    /// it: most used first. This is the project's vocabulary that agents reuse.
    pub fn project_labels(&self, project: &str) -> Result<Vec<(String, usize)>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for issue in self.list_issues(Some(project), true)? {
            if issue.origin == "nitely" {
                for l in issue.labels {
                    *counts.entry(l).or_insert(0) += 1;
                }
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Adds and removes labels of a native issue. Removing one it does not carry is not an error.
    pub fn change_labels(&mut self, key: &str, add: &[String], remove: &[String], actor: &Actor) -> Result<IssueRecord> {
        let (add, remove) = (normalize_labels(add)?, normalize_labels(remove)?);
        let key = self.current_key(key)?;
        // Read and write under one immediate transaction, so two agents labelling the same issue
        // at once both keep their changes.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (key, origin, stored): (String, String, String) = tx
            .query_row("SELECT key, origin, labels FROM issue WHERE key = ?1 COLLATE NOCASE", [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
            .with_context(|| format!("no issue {key}"))?;
        if origin != "nitely" {
            bail!("{key} is mirrored from {origin}; change its labels there");
        }
        // Labels stored before they were normalized (an import's "Bug") count as their normal form.
        let before: Vec<String> = stored_labels(&stored);
        let mut labels: Vec<String> = before.iter().filter(|l| !remove.contains(l)).cloned().collect();
        for l in &add {
            if !labels.contains(l) {
                labels.push(l.clone());
            }
        }
        let raw: Vec<String> = serde_json::from_str(&stored).unwrap_or_default();
        if labels != raw {
            let added: Vec<String> = labels.iter().filter(|l| !before.contains(l)).map(|l| format!("+{l}")).collect();
            let removed: Vec<String> = before.iter().filter(|l| !labels.contains(l)).map(|l| format!("-{l}")).collect();
            let ts = now();
            tx.execute("UPDATE issue SET labels = ?2, updated_at = ?3 WHERE key = ?1", params![key, serde_json::to_string(&labels)?, iso(ts)])?;
            if !added.is_empty() || !removed.is_empty() {
                tx.execute(
                    "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'labels_changed', ?4)",
                    params![ts, actor.id(), key, format!("Labels {}", [added, removed].concat().join(" "))],
                )?;
            }
        }
        tx.commit()?;
        self.issue(&key)?.context("issue vanished")
    }

    /// Makes an issue the human's own work, or hands it back to agents. Any agent may give an issue
    /// to the human; only the human hands one back. Giving an open issue to the human releases its
    /// claim and puts it in Needs You, unless it waits in Backlog; handing an open one back puts it
    /// in Todo for the next agent, unless it waits in Backlog. A state it changes is logged as a
    /// move, so its track follows.
    pub fn set_owner(&mut self, key: &str, owner: &str, actor: &Actor) -> Result<IssueRecord> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.origin != "nitely" {
            bail!("{} is mirrored from {}; change it there", issue.key, issue.origin);
        }
        let owner = owner.trim();
        self.check_owner_change(key, owner)?;
        if !OWNERS.contains(&owner) {
            bail!("owner must be one of {}", OWNERS.join(", "));
        }
        if owner == issue.owner {
            return Ok(issue);
        }
        if owner == "agent" && *actor != Actor::Human {
            bail!("only the human hands their own work back to agents; `ask` them if an agent should do {}", issue.key);
        }
        let to_human = owner == "human";
        let ready_for_human = to_human && issue.is_open() && issue.state != "Backlog" && !issue.needs_human();
        let ready_for_agents = !to_human && issue.is_open() && !matches!(issue.state.as_str(), "Backlog" | "Todo");
        let (state, state_type) = match (ready_for_human, ready_for_agents) {
            (true, _) => ("Needs You", "started"),
            (_, true) => ("Todo", "unstarted"),
            _ => (issue.state.as_str(), issue.state_type.as_str()),
        };
        let ts = now();
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE issue SET owner = ?2, updated_at = ?3, state = ?4, state_type = ?5,
               claimed_by = CASE WHEN ?6 THEN NULL ELSE claimed_by END,
               claimed_at = CASE WHEN ?6 THEN NULL ELSE claimed_at END
             WHERE key = ?1",
            params![issue.key, owner, iso(ts), state, state_type, to_human || ready_for_agents],
        )?;
        let summary = if to_human { "Made the human's own work" } else { "Handed back to agents" };
        tx.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'owner_changed', ?4)",
            params![ts, actor.id(), issue.key, summary],
        )?;
        if state != issue.state {
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'issue_moved', ?4)",
                params![ts, actor.id(), issue.key, format!("{} → {state}", issue.state)],
            )?;
        }
        tx.commit()?;
        self.issue(&issue.key)?.context("issue vanished")
    }

    /// A progress line, handoff note or comment on an issue's timeline. Refreshes a claim it holds.
    pub fn add_note(&self, key: &str, text: &str, kind: &str, actor: &Actor) -> Result<()> {
        self.add_note_from(key, text, kind, actor, actor.id())
    }

    /// `add_note`, written under `session` (the Codex thread an agent's report came from) while
    /// `actor` still keeps its claim fresh.
    pub fn add_note_from(&self, key: &str, text: &str, kind: &str, actor: &Actor, session: &str) -> Result<()> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if text.trim().is_empty() {
            bail!("a note needs text");
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, session, issue.key, kind, text.trim()],
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
        // Only open issues need their timeline. Every event that names an issue, in order, matched
        // here rather than joined in SQL: a case-insensitive join to `issue` cannot use an index,
        // and SQLite then compares every event with every issue (seconds once agents have been busy).
        let open: HashSet<String> = issues.iter().filter(|i| i.is_open()).map(|i| i.key.to_uppercase()).collect();
        let mut timelines: HashMap<String, Vec<TimelineEntry>> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, ts, actor, type, summary FROM event WHERE issue_key IS NOT NULL ORDER BY ts, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, TimelineEntry { ts: r.get(1)?, actor: r.get(2)?, kind: r.get(3)?, summary: r.get(4)? }))
        })?;
        for row in rows {
            let (key, entry) = row?;
            let key = key.to_uppercase();
            if open.contains(&key) {
                timelines.entry(key).or_default().push(entry);
            }
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
            proposal: self.proposal_for_issue(&issue.key)?,
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
    fn the_humans_own_work_is_never_handed_out() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:a".into());
        let mut buy = new("Buy a test phone");
        buy.owner = Some("human".into());
        buy.priority = 1;
        let buy = db.create_issue(&buy, &agent).unwrap();
        assert_eq!((buy.owner.as_str(), buy.state.as_str()), ("human", "Needs You"), "filed straight to the human");
        let mut bad = new("Something else entirely");
        bad.owner = Some("robot".into());
        assert!(db.create_issue(&bad, &agent).is_err());

        // Moved back to Todo, it is still not handed out.
        db.move_issue(&buy.key, "Todo", None, None, &Actor::Human).unwrap();
        assert!(db.next_task("NIT", "claude:a").unwrap().is_none());

        // An agent may give its claimed issue to the human; only the human hands it back.
        let code = db.create_issue(&new("Wire the store console checks"), &agent).unwrap();
        assert_eq!(db.next_task("NIT", "claude:a").unwrap().unwrap().key, code.key);
        let given = db.set_owner(&code.key, "human", &agent).unwrap();
        assert_eq!((given.state.as_str(), given.claimed_by.as_deref()), ("Needs You", None));
        assert!(db.set_owner(&code.key, "agent", &agent).unwrap_err().to_string().contains("only the human"));
        let back = db.set_owner(&code.key, "agent", &Actor::Human).unwrap();
        assert_eq!((back.owner.as_str(), back.state.as_str()), ("agent", "Todo"), "handed back for the next agent");
        assert_eq!(back.track.station, "todo", "the track follows the moves");
        assert_eq!(db.next_task("NIT", "claude:a").unwrap().unwrap().key, code.key);

        // A Backlog issue stays in Backlog when it becomes the human's.
        let mut later = new("Sign the hosting contract");
        later.state = Some("Backlog".into());
        let later = db.create_issue(&later, &agent).unwrap();
        assert_eq!(db.set_owner(&later.key, "human", &agent).unwrap().state, "Backlog");
        let kinds: Vec<String> = db.timeline(&code.key).unwrap().into_iter().map(|t| t.kind).collect();
        assert_eq!(kinds.iter().filter(|k| *k == "owner_changed").count(), 2);

        // The human took it up themselves (In Progress); handing it back still readies it for agents.
        let mut own = new("Order the build server");
        own.owner = Some("human".into());
        let own = db.create_issue(&own, &agent).unwrap();
        db.move_issue(&own.key, "In Progress", None, None, &Actor::Human).unwrap();
        assert_eq!(db.set_owner(&own.key, "agent", &Actor::Human).unwrap().state, "Todo");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn labels_are_normalized_counted_and_changed() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:a".into());
        assert_eq!(normalize_label("  Roku TV "), Some("roku-tv".into()));
        assert_eq!(normalize_label("roku_tv"), Some("roku-tv".into()));
        assert_eq!(normalize_label("--"), None);
        assert_eq!(normalize_label("İstanbul"), Some("istanbul".into()));
        for l in ["İstanbul", "Roku TV", "ÉTÉ", "a/b.c+d"] {
            let once = normalize_label(l).unwrap();
            assert_eq!(normalize_label(&once).as_deref(), Some(once.as_str()), "normalizing {l} twice changes nothing");
        }

        let a = db.create_issue(&NewIssue { labels: vec!["Roku".into(), "roku".into(), "Billing".into()], ..new("Channel store listing") }, &agent).unwrap();
        assert_eq!(a.labels, ["roku", "billing"], "normalized, each once");
        db.create_issue(&NewIssue { labels: vec!["roku".into()], ..new("Remote control focus order") }, &agent).unwrap();
        assert!(db.create_issue(&NewIssue { labels: vec!["!!".into()], ..new("Something unrelated to it") }, &agent).is_err());
        assert_eq!(db.project_labels("NIT").unwrap(), [("roku".to_string(), 2), ("billing".to_string(), 1)]);

        // An import kept "Bug" as it was; it reads, filters and is removed as "bug". One with no
        // normal form (too long) survives other edits as it is.
        let long = "x".repeat(41);
        db.conn.execute(&format!("UPDATE issue SET labels = '[\"Roku\",\"Bug\",\"billing\",\"{long}\"]' WHERE key = ?1"), [&a.key]).unwrap();
        assert_eq!(db.issue(&a.key).unwrap().unwrap().labels, ["roku", "bug", "billing", long.as_str()]);
        assert_eq!(db.change_labels(&a.key, &[], &["bug".into()], &agent).unwrap().labels, ["roku", "billing", long.as_str()]);
        db.conn.execute("UPDATE issue SET labels = '[\"Roku\",\"billing\"]' WHERE key = ?1", [&a.key]).unwrap();
        let changed = db.change_labels(&a.key, &["Tizen".into()], &["billing".into(), "absent".into()], &agent).unwrap();
        assert_eq!(changed.labels, ["roku", "tizen"]);
        let last = db.timeline(&a.key).unwrap().pop().unwrap();
        assert_eq!((last.kind.as_str(), last.summary.as_deref()), ("labels_changed", Some("Labels +tizen -billing")));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_duplicate_check_reads_bodies_and_closed_issues() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:a".into());
        let process = db.create_issue(&new("Document and codify the deploy process"), &agent).unwrap();
        // Sharing only "deploy" with it is not a duplicate.
        db.create_issue(&new("Deploy /me/entitlements/ to production"), &agent).unwrap();

        // Work already done is not filed again, and the refusal says it is closed and why.
        let tray = NewIssue {
            body: "src-tauri/src/tray.rs builds the count in count_needs and skips rows with a rebase in progress.".into(),
            ..new("Tray badge ignores stuck worktrees")
        };
        let done = db.create_issue(&tray, &agent).unwrap();
        db.move_issue(&done.key, "Done", Some("fixed"), None, &agent).unwrap();
        let again = NewIssue { body: "count_needs in src-tauri/src/tray.rs only looks at sessions.".into(), ..new("Stuck worktrees missing from the tray count") };
        let refused = db.create_issue(&again, &agent).unwrap_err().to_string();
        assert!(refused.contains(&format!("{} Tray badge ignores stuck worktrees (closed: Done)", done.key)), "{refused}");
        assert!(refused.contains("both name count_needs, src-tauri/src/tray.rs"), "{refused}");
        assert!(!refused.contains(&process.key), "{refused}");
        assert!(db.create_issue(&NewIssue { allow_similar: true, ..again }, &agent).is_ok(), "allow_similar still files it");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn files_a_batch_in_dependency_order_with_local_refs() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:a".into());
        let existing = db.create_issue(&new("Wire the settings page"), &agent).unwrap();
        let item = |r: &str, title: &str, blocked_by: &[&str]| BatchItem {
            local_ref: r.into(),
            issue: NewIssue { blocked_by: blocked_by.iter().map(|b| b.to_string()).collect(), ..new(title) },
        };

        // "ui" comes first but waits on "api", which waits on "schema" and an existing issue.
        let out = db
            .create_issues(
                &[
                    item("ui", "Show invoices in the account view", &["api"]),
                    item("api", "Invoice endpoint for the account", &["schema", &existing.key]),
                    item("schema", "Store invoices with their line items", &[]),
                ],
                &agent,
            )
            .unwrap();
        assert_eq!(out.filed.iter().map(|f| f.local_ref.as_str()).collect::<Vec<_>>(), ["schema", "api", "ui"]);
        let key = |r: &str| out.keys[r].clone();
        assert_eq!(db.issue(&key("api")).unwrap().unwrap().blocked_by, vec![existing.key.clone(), key("schema")]);
        assert_eq!(db.issue(&key("ui")).unwrap().unwrap().blocked_by, vec![key("api")]);
        assert!(out.skipped.is_empty());

        // Cycles, unknown refs, a missing ref and an invalid item file nothing.
        let before = db.list_issues(Some("NIT"), true).unwrap().len();
        let cycle = db.create_issues(&[item("a", "Alpha thing to build", &["b"]), item("b", "Beta thing to build", &["a"])], &agent);
        assert!(cycle.unwrap_err().to_string().contains("cycle"));
        let unknown = db.create_issues(&[item("a", "Alpha thing to build", &["nope"])], &agent);
        assert!(format!("{:#}", unknown.unwrap_err()).contains("no issue nope"));
        assert!(db.create_issues(&[item("", "Alpha thing to build", &[])], &agent).is_err());
        let mut bad = item("b", "Beta thing to build", &[]);
        bad.issue.kind = "chore".into();
        assert!(db.create_issues(&[item("a", "Alpha thing to build", &[]), bad], &agent).is_err());
        assert_eq!(db.list_issues(Some("NIT"), true).unwrap().len(), before, "nothing was filed");

        // A likely duplicate is skipped, and what named it waits on the issue it duplicates.
        let out = db
            .create_issues(&[item("dup", "Wire up the settings page", &[]), item("next", "Settings page keyboard shortcuts", &["dup"])], &agent)
            .unwrap();
        assert_eq!(out.skipped.len(), 1);
        assert_eq!((out.skipped[0].local_ref.as_str(), out.skipped[0].stands_for.as_str()), ("dup", existing.key.as_str()));
        assert_eq!(db.issue(&out.keys["next"]).unwrap().unwrap().blocked_by, vec![existing.key.clone()]);
        // ...unless it allows similar; and a near-copy of an earlier item in the batch is skipped too.
        let mut forced = item("again", "Wire up the settings page", &[]);
        forced.issue.allow_similar = true;
        let out = db.create_issues(&[forced, item("twice", "Wire up the settings pages", &[])], &agent).unwrap();
        assert_eq!(out.filed.len(), 1);
        assert_eq!(out.skipped[0].stands_for, out.keys["again"]);
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
        let linear = IssueRecord { key: "OPS-5".into(), title: "Upstream fix".into(), state: "Todo".into(), state_type: "unstarted".into(), ..Default::default() };
        db.replace_issues("linear", &[linear]).unwrap();
        let mut waits = new("Use the upstream fix");
        waits.blocked_by = vec!["OPS-5".into()];
        assert!(db.create_issue(&waits, &h).is_err());
        db.create_issue(&new("Use the upstream fix"), &h).unwrap();
        assert!(db.add_blocker("NIT-1", "OPS-5", &h).is_err());
        assert!(db.add_blocker("OPS-5", "NIT-1", &h).is_err());
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
        assert_eq!(parse_state("needs-you").unwrap().0, "Needs You");
    }
}
