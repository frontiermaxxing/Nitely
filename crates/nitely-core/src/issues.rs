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
/// How much verification a change is expected to need, least to most. The app shows these as
/// Basic, Focused, Integration and Full. None stored means undeclared: today's gate, never Basic.
pub const REVIEW_LEVELS: &[&str] = &["inert", "contained", "standard", "sensitive"];
/// Who set a review level. An implementer's declaration on an issue it holds is only a hint.
pub const REVIEW_SOURCES: &[&str] = &["author", "planner", "coordinator", "human", "implementer"];

/// Where `level` sits, least to most. None when it is not a review level.
pub fn review_rank(level: &str) -> Option<usize> {
    REVIEW_LEVELS.iter().position(|l| *l == level)
}

/// The name the app shows for a stored level.
pub fn review_depth(level: &str) -> &'static str {
    match level {
        "inert" => "Basic",
        "contained" => "Focused",
        "standard" => "Integration",
        "sensitive" => "Full",
        _ => "Undeclared",
    }
}

pub fn check_review_level(level: &str) -> Result<()> {
    if review_rank(level).is_none() {
        bail!("review level is one of {}, not {level}", REVIEW_LEVELS.join(", "));
    }
    Ok(())
}

/// The level to store. `protected` is why a path rule applies (a human or blocked path); that
/// raises the declaration to sensitive. The note is set when that raise wins.
pub fn resolve_review_level(declared: Option<&str>, protected: Option<&str>) -> Result<(Option<String>, Option<String>)> {
    let declared = declared.map(str::trim).filter(|s| !s.is_empty() && *s != "undeclared");
    if let Some(d) = declared {
        check_review_level(d)?;
    }
    let Some(why) = protected.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((declared.map(str::to_string), None));
    };
    let at_top = declared.is_some_and(|d| review_rank(d) == review_rank("sensitive"));
    let stored = if at_top { declared.unwrap() } else { "sensitive" };
    let note = if at_top { None } else { Some(match declared {
        Some(d) => format!("{d} raised: {why}"),
        None => format!("undeclared raised: {why}"),
    }) };
    Ok((Some(stored.to_string()), note))
}

fn check_review_change(old: Option<&str>, level: Option<&str>, actor: &Actor) -> Result<()> {
    if let Some(level) = level {
        check_review_level(level)?;
    }
    if !matches!(actor, Actor::Human) {
        let level = level.context("only the person clears a review level")?;
        if let Some(old) = old {
            if review_rank(level).unwrap_or(0) < review_rank(old).unwrap_or(0) {
                bail!("a review level can only be raised from {old}; the person sets a lower one");
            }
        }
    }
    Ok(())
}

fn review_change_summary(from: Option<&str>, to: Option<&str>, source: &str, note: Option<&str>) -> String {
    let name = |l: Option<&str>| match l {
        None | Some("") => "undeclared".to_string(),
        Some(l) => format!("{} ({l})", review_depth(l)),
    };
    let hint = if source == "implementer" { ", a hint" } else { "" };
    let raised = note.map(|n| format!("; {n}")).unwrap_or_default();
    format!("Review level: {} → {} ({source}{hint}){raised}", name(from), name(to))
}

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

/// What `claim_for_start` reads of an issue: key, state, claim holder and time, origin, owner.
type StartRow = (String, String, Option<String>, Option<i64>, String, String);

/// A Todo or In Progress issue `next_task` may take: key, priority, number, whether it plans a goal
/// (0) or not (1), its goal's place, and the goal's target date.
type Candidate = (String, i64, i64, i64, i64, Option<String>);

/// Sessions seen within the claim TTL of `now`, as (folder, last seen): the only ones that can
/// keep an issue from going idle.
pub(crate) fn recent_sessions(conn: &rusqlite::Connection, now: i64) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT cwd, last_seen_at FROM session WHERE last_seen_at >= ?1")?;
    let rows = stmt.query_map([now - CLAIM_TTL_SECS], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// When an In Progress issue that no live claim holds last showed any work, if that is longer ago
/// than the claim TTL: its newest event, or the newest of `sessions` in a worktree started for it.
/// Most work starts with start_task and holds no claim, so a missing claim alone says nothing; one
/// that also shows no work was left, and would otherwise read as work in progress forever, never
/// offered again (NIT-275). Some(0) when it never showed any.
pub(crate) fn idle_since(conn: &rusqlite::Connection, key: &str, sessions: &[(String, i64)], now: i64) -> Result<Option<i64>> {
    let row: Option<(String, String, String, Option<i64>)> = conn
        .query_row("SELECT state, origin, owner, claimed_at FROM issue WHERE key = ?1 COLLATE NOCASE", [key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let Some((state, origin, owner, claimed_at)) = row else { return Ok(None) };
    let since = now - CLAIM_TTL_SECS;
    if state != "In Progress" || origin != "nitely" || owner == "human" || claimed_at.is_some_and(|c| c >= since) {
        return Ok(None);
    }
    let last: Option<i64> = conn.query_row("SELECT MAX(ts) FROM event WHERE issue_key = ?1 COLLATE NOCASE", [key], |r| r.get(0))?;
    if last.is_some_and(|t| t >= since) {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT DISTINCT cwd FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'worktree' AND cwd IS NOT NULL")?;
    let worktrees: Vec<String> = stmt.query_map([key], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let seen = sessions.iter().filter(|(cwd, _)| worktrees.iter().any(|w| crate::paths::is_within(cwd, w))).map(|(_, at)| *at).max();
    let last = last.unwrap_or(0).max(seen.unwrap_or(0));
    Ok((last < since).then_some(last))
}

/// What the track says of an issue `idle_since` found left.
fn idle_stuck(last: i64, now: i64) -> String {
    if last == 0 {
        "No activity".to_string()
    } else {
        format!("No activity for {} h", (now - last) / 3600)
    }
}

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
    /// The goal it belongs to (`goals.rs`), if any.
    pub goal: Option<i64>,
    /// Declared review level: inert, contained, standard or sensitive. None is undeclared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_level: Option<String>,
    /// Who declared `review_level` (`Actor::id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_level_by: Option<String>,
    /// author, planner, coordinator, human, or implementer (a hint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_level_source: Option<String>,
    /// When `review_level` was last set, unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_level_at: Option<i64>,
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
    pub verification: Vec<crate::verification::Plan>,
    pub issue: IssueRecord,
    pub relations: Relations,
    pub timeline: Vec<TimelineEntry>,
    pub prs: Vec<crate::db::PrRecord>,
    pub reviews: Vec<crate::submit::ReviewRecord>,
    /// The playbook change the issue carries, when an agent proposed one.
    pub proposal: Option<crate::proposals::Proposal>,
    /// The goal plan the issue carries, when an agent drafted one (goal_plans.rs).
    pub goal_plan: Option<crate::goal_plans::GoalPlanView>,
    /// Its review batches and trips through a land queue (NIT-218).
    pub batches: crate::batches::IssueBatches,
    /// The asks about it, answered or not, so its timeline can link each one (NIT-546).
    pub asks: Vec<Ask>,
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
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineEntry {
    pub ts: i64,
    /// The session that wrote it (`claude:<session id>`, `codex:<thread id>`), or for an older
    /// event or an agent without hooks its agent and folder (`claude:c:/code/app`), or `human`.
    pub actor: String,
    pub kind: String,
    pub summary: Option<String>,
    /// The folder its session works in, where known: what a session id does not say (NIT-194).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
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
    /// Why it closed unanswered: its issue closed first ("closed with NIT-3 (Done)").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed: Option<String>,
    /// The review finding the agent asks the person to accept as it is (`request_acceptance`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<crate::accept::AskedFinding>,
    /// When the answer reached an agent, and which (NIT-547): the one that asked, or the one that
    /// took the issue up after it had gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_to: Option<String>,
}

impl Ask {
    /// The line that tells the agent that asked its answer, on its next nitely call (NIT-277).
    pub fn outcome(&self) -> String {
        let about = self.issue_key.as_deref().map(|k| format!("{k}: ")).unwrap_or_default();
        format!("{about}the person answered ask {} (\"{}\"): {}", self.id, self.question, self.answer.as_deref().unwrap_or(""))
    }
}

/// An agent as a person reads it: its vendor ("Codex"), not its id ("codex:c:/users/.../demo").
pub(crate) fn agent_name(id: &str) -> String {
    match id.split(':').next().unwrap_or(id) {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        "cursor" => "Cursor".into(),
        "human" => "You".into(),
        other if !other.is_empty() => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
        _ => "An agent".into(),
    }
}

/// The first sentence of `text`, at most `max` characters, cut at a word with an ellipsis: what a
/// title shows of a longer reason.
pub(crate) fn headline(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    // A sentence ends at ". ", "? " or "! " (not inside "e.g." or a version like 1.2).
    let end = line.char_indices().find(|&(i, c)| matches!(c, '.' | '?' | '!') && line[i + 1..].starts_with(' ') && !line[..i].ends_with("e.g") && !line[..i].ends_with("i.e")).map(|(i, _)| i);
    let sentence = match end {
        Some(i) => &line[..i],
        None => line.trim_end_matches(['.', '!', '?']),
    };
    if sentence.chars().count() <= max {
        return sentence.to_string();
    }
    let cut: String = sentence.chars().take(max - 1).collect();
    let at_word = match cut.rfind(' ') {
        Some(i) if i > max / 3 => cut[..i].trim_end_matches([',', ';', ':', ' ']).to_string(),
        _ => cut,
    };
    format!("{at_word}…")
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
    /// An active goal of the same project to file it into.
    pub goal: Option<i64>,
    /// inert, contained, standard or sensitive. New issues default to standard.
    pub review_level: Option<String>,
    /// author, planner, coordinator, human or implementer. author when a level is set and this is empty.
    pub review_source: Option<String>,
    /// Set when a path rule raised `review_level`. Goes on the timeline, not its own column.
    pub review_note: Option<String>,
}

/// A batch that passed its checks: each item's ref and checked issue, its blockers split into items
/// of the batch and existing issues, and the order to file it in (blockers first).
struct PreparedBatch<'a> {
    refs: Vec<&'a str>,
    checked: Vec<CheckedIssue>,
    local: Vec<Vec<usize>>,
    existing: Vec<Vec<String>>,
    order: Vec<usize>,
}

/// Priorities by number, as the timeline names them: 0 is none.
const PRIORITY_NAMES: [&str; 5] = ["none", "Urgent", "High", "Medium", "Low"];

/// The most issues one `file_issues` call (or `nitely issue create-many`) files (NIT-235). An agent
/// writing a hundred items in one call works for a long time with nothing on the board; batches of
/// ten show up as it goes. A goal plan is not held to it: the person approves it as one decision.
pub const FILE_ISSUES_MAX: usize = 10;

/// Refuses a batch of more than `FILE_ISSUES_MAX` items, before anything is filed.
pub fn check_batch_size(items: usize) -> Result<()> {
    if items > FILE_ISSUES_MAX {
        bail!(
            "{items} items, but file_issues takes at most {FILE_ISSUES_MAX} per call: file the first {FILE_ISSUES_MAX} now and the rest in further calls. \
Refs are local to one call, so file blockers first; a later batch names them in blocked_by by the keys the earlier call returned"
        );
    }
    Ok(())
}

/// One issue of a `file_issues` batch: a new issue, and the ref the batch's other items name it by
/// in their `blocked_by`.
#[derive(Debug, Clone, Default)]
pub struct BatchItem {
    pub local_ref: String,
    pub issue: NewIssue,
    /// Issues the duplicate check passes over for this item: a goal plan's own issues, which
    /// describe the very work it files.
    pub not_duplicates: Vec<String>,
    /// The batch's items are distinct by design (a goal plan's cards, which the person reads and
    /// edits together), so they are not checked as duplicates of one another.
    pub distinct_in_batch: bool,
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
    goal: Option<i64>,
    state: &'static str,
    state_type: &'static str,
    review_level: Option<String>,
    review_source: String,
    review_note: Option<String>,
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
           priority, evidence, created_at, updated_at, owner, labels, goal,
           review_level, review_level_by, review_level_source, review_level_at)
         VALUES (?1, 'nitely', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
            serde_json::to_string(&c.labels)?,
            c.goal,
            c.review_level,
            c.review_level.as_ref().map(|_| actor.id()),
            c.review_level.as_ref().map(|_| c.review_source.as_str()),
            c.review_level.as_ref().map(|_| ts),
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
    if let Some(level) = &c.review_level {
        tx.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'review_level_changed', ?4)",
            params![ts, actor.id(), key, review_change_summary(None, Some(level), &c.review_source, c.review_note.as_deref())],
        )?;
    }
    Ok(key)
}

const ISSUE_COLUMNS: &str = "key, origin, project, title, body, url, state, state_type, kind, source,
    priority, labels, evidence, claimed_by, created_at, updated_at, owner, goal,
    review_level, review_level_by, review_level_source, review_level_at";

/// The scheme the app registers, so a link to an issue opens it in Nitely.
pub const LINK_SCHEME: &str = "nitely";

/// `nitely://issue/NIT-14`: the app's own link to a native issue, which the OS opens in the app even
/// when it is closed. Agents write the web form (`links::issue_url`), which chats and GitHub open.
pub fn issue_link(key: &str) -> String {
    format!("{LINK_SCHEME}://issue/{}", key.to_uppercase())
}

/// The issue key a link names, in either form: `nitely://issue/NIT-14` or the app's loopback
/// `http://127.0.0.1:7423/i/NIT-14` (`links::key_from_url`).
pub fn key_from_link(link: &str) -> Option<String> {
    crate::links::key_from_url(link)
}

/// What a `nitely://` or loopback link opens: an issue or an ask (NIT-546).
pub fn target_from_link(link: &str) -> Option<crate::links::Target> {
    crate::links::target_from_url(link)
}

fn issue_from_row(r: &Row) -> rusqlite::Result<IssueRecord> {
    let origin: String = r.get(1)?;
    let key: String = r.get(0)?;
    let stored: String = r.get(5)?;
    // A native issue's link is always the app's own. What is stored for it is where it came from
    // (an imported Linear issue's address), shown beside it but never handed out as its link.
    let (url, external_url) = if origin == "nitely" { (crate::links::issue_url(&key), stored) } else { (stored, String::new()) };
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
        goal: r.get(17)?,
        review_level: r.get(18)?,
        review_level_by: r.get(19)?,
        review_level_source: r.get(20)?,
        review_level_at: r.get(21)?,
        ..Default::default()
    })
}

/// 0 (none) sorts after 4 (low).
/// Where a ready issue goes in `next_task`'s order, first to last (NIT-547): one answered while its
/// agent was gone first, whatever its priority, since a person is waiting on it to move; then by
/// urgency, a goal's planning issue, its goal's place and target date, and its number.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PickOrder {
    not_answered: bool,
    pub(crate) urgency: i64,
    planning: i64,
    goal: i64,
    target: Option<String>,
    pub(crate) number: i64,
}

pub(crate) fn pick_order(answered: bool, urgency: i64, planning: i64, goal: i64, target: Option<String>, number: i64) -> PickOrder {
    PickOrder { not_answered: !answered, urgency, planning, goal, target, number }
}

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

/// Refuses a priority outside 0 (none) to 4 (low): checked before an edit that also sets one
/// writes anything, so a bad priority leaves the whole edit undone.
pub fn check_priority(priority: i64) -> Result<()> {
    if !(0..=4).contains(&priority) {
        bail!("priority is 0 (none), 1 (urgent), 2 (high), 3 (medium) or 4 (low)");
    }
    Ok(())
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
        Ok(Project { key, name: name.trim().into(), open_issues: 0, archived: false })
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        Ok(self.all_projects()?.into_iter().filter(|p| !p.archived).collect())
    }

    /// Includes archived projects for explicit lookup and restoration.
    pub fn all_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.key, p.name,
               (SELECT COUNT(*) FROM issue i WHERE i.project = p.key
                  AND i.state_type NOT IN ('completed', 'canceled')), p.archived
             FROM project p ORDER BY p.key",
        )?;
        let rows = stmt.query_map([], |r| Ok(Project { key: r.get(0)?, name: r.get(1)?, open_issues: r.get(2)?, archived: r.get(3)? }))?;
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
        Ok(self.all_projects()?.into_iter().find(|p| p.key.eq_ignore_ascii_case(key.trim())))
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
    /// Checks a batch as `create_issues` would before filing anything (refs, each item, blockers
    /// and cycles), without filing it.
    pub fn check_batch(&self, items: &[BatchItem]) -> Result<()> {
        self.prepare_batch(items).map(|_| ())
    }

    fn prepare_batch<'a>(&self, items: &'a [BatchItem]) -> Result<PreparedBatch<'a>> {
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

        Ok(PreparedBatch { refs, checked, local, existing, order })
    }

    pub fn create_issues(&mut self, items: &[BatchItem], actor: &Actor) -> Result<BatchFiled> {
        let PreparedBatch { refs, checked, local, existing, order } = self.prepare_batch(items)?;

        // Duplicates of open issues are found before the transaction; of earlier items, as it goes.
        let mut duplicates: Vec<Vec<Duplicate>> = Vec::with_capacity(items.len());
        for (item, c) in items.iter().zip(&checked) {
            duplicates.push(if item.issue.allow_similar {
                Vec::new()
            } else {
                self.likely_duplicates(&c.project, &item.issue)?.into_iter().filter(|d| !item.not_duplicates.contains(&d.issue.key)).collect()
            });
        }

        let ts = now();
        let mut out = BatchFiled::default();
        // The key each ref stands for: the issue filed for it, or the one it duplicates.
        let mut key_of: Vec<Option<String>> = vec![None; items.len()];
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &i in &order {
            let (item, c) = (&items[i], &checked[i]);
            let earlier = if item.issue.allow_similar || item.distinct_in_batch {
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
                url: crate::links::issue_url(&key),
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
        if let Some(goal) = new.goal {
            self.goal_for(Some(&project.key), goal)?;
        }
        let review_level = match new.review_level.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "undeclared") {
            None => Some("standard".to_string()),
            Some(level) => {
                check_review_level(level)?;
                Some(level.to_string())
            }
        };
        let review_source = if review_level.is_none() {
            String::new()
        } else {
            let source = new.review_source.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("author");
            if !REVIEW_SOURCES.contains(&source) {
                bail!("review source must be one of {}", REVIEW_SOURCES.join(", "));
            }
            source.to_string()
        };
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
            goal: new.goal,
            state,
            state_type,
            review_level,
            review_source,
            review_note: new.review_note.clone(),
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
                if i.track.stuck.is_none() && i.state == "In Progress" {
                    let ts = now();
                    if let Some(last) = idle_since(&self.conn, &i.key, &recent_sessions(&self.conn, ts)?, ts)? {
                        i.track.stuck = Some(idle_stuck(last, ts));
                    }
                }
                Some(i)
            }
            None => None,
        })
    }

    /// Every issue (native and mirrored), most urgent first; `project` narrows to one project's
    /// native issues.
    pub fn list_issues(&self, project: Option<&str>, include_closed: bool) -> Result<Vec<IssueRecord>> {
        self.read_issues(project, include_closed, false)
    }

    /// Lifecycle processing must still see archived work already in flight.
    pub(crate) fn all_issues(&self, include_closed: bool) -> Result<Vec<IssueRecord>> {
        self.read_issues(None, include_closed, true)
    }

    fn read_issues(&self, project: Option<&str>, include_closed: bool, include_archived: bool) -> Result<Vec<IssueRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ISSUE_COLUMNS} FROM issue
             WHERE (?1 IS NULL OR project = ?1 COLLATE NOCASE)
               AND (?3 OR ?1 IS NOT NULL OR key IN (SELECT key FROM active_issue))
               AND (?2 OR state_type NOT IN ('completed', 'canceled'))
             ORDER BY CASE priority WHEN 0 THEN 5 ELSE priority END, number, key"
        ))?;
        let rows = stmt.query_map(params![project, include_closed, include_archived], issue_from_row)?;
        let mut issues: Vec<IssueRecord> = rows.collect::<rusqlite::Result<_>>()?;
        self.hydrate_issue_list(&mut issues)?;
        Ok(issues)
    }

    /// Goal rows only need issues belonging to the selected goals. Hydrate them together so
    /// shared dependencies and timelines are read once, even across many projects.
    pub(crate) fn goal_issues(&self, ids: &[i64]) -> Result<Vec<IssueRecord>> {
        if ids.is_empty() { return Ok(Vec::new()); }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ISSUE_COLUMNS} FROM issue WHERE goal IN (SELECT value FROM json_each(?1))
             ORDER BY CASE priority WHEN 0 THEN 5 ELSE priority END, number, key"
        ))?;
        let rows = stmt.query_map([serde_json::to_string(ids)?], issue_from_row)?;
        let mut issues = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        self.hydrate_issue_list(&mut issues)?;
        Ok(issues)
    }

    fn hydrate_issue_list(&self, issues: &mut [IssueRecord]) -> Result<()> {
        let deps = Deps::load(&self.conn)?;
        for i in issues.iter_mut() { deps.fill(i); }
        self.fill_tracks(issues)?;
        issues.sort_by_key(|i| rank(i.urgency));
        Ok(())
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
        if state == "Done" {
            // A change handed in (submitted) and not landed since is not on the integration branch:
            // Done would close the issue with the work left on its branch.
            let (submitted, landed): (Option<i64>, Option<i64>) = self.conn.query_row(
                "SELECT (SELECT MAX(id) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'submit'),
                        (SELECT MAX(id) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'landed')",
                [&issue.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let unlanded = submitted.is_some_and(|s| landed.is_none_or(|l| l < s));
            if unlanded {
                bail!(
                    "{}'s change was submitted and never landed, so it is not on the integration branch: land it, or Cancel the issue with a reason to close it without landing. If its commits reached the integration branch another way (merged by hand, or rebased into another change), Done takes it once that branch holds every commit of its last submit",
                    issue.key
                );
            }
        }
        if matches!(state, "Canceled" | "Duplicate") && !given(reason) {
            bail!("moving to {state} needs a reason");
        }
        Ok(())
    }

    /// Refuses Done, asked for directly (the app, MCP, the CLI), while a question on the issue is
    /// open: it is answered first, or the issue canceled to drop it, never dropped silently by a
    /// Done (NIT-220). Nitely's own closing moves decide their asks themselves: a land closes what
    /// was asked about the change, an approved playbook change or goal plan answers its issue.
    pub fn check_open_asks(&self, key: &str, to: &str) -> Result<()> {
        // As move_issue reads the state, so no spelling of Done gets past.
        if parse_state(to)?.0 != "Done" {
            return Ok(());
        }
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        let open: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM ask WHERE issue_key = ?1 COLLATE NOCASE AND answer IS NULL AND closed IS NULL",
            [&issue.key],
            |r| r.get(0),
        )?;
        if open > 0 {
            let (s, it) = if open == 1 { ("", "it") } else { ("s", "them") };
            bail!("{} has {open} open question{s}: answer {it} first (Cancel the issue with a reason to drop {it} with it)", issue.key);
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
        if let Some(moved) = self.before_plan_move(&issue.key, state, reason, actor)? {
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
        // An open question about work that just closed has nothing left to decide: it closes too,
        // so the Inbox and the phone never ask about finished work.
        if issue.is_open() && matches!(state_type, "completed" | "canceled") {
            let why = format!("closed with {} ({state})", issue.key);
            let open: Vec<(i64, String)> = {
                let mut stmt = tx.prepare("SELECT id, question FROM ask WHERE issue_key = ?1 COLLATE NOCASE AND answer IS NULL AND closed IS NULL")?;
                let rows = stmt.query_map([&issue.key], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (id, question) in open {
                tx.execute("UPDATE ask SET closed = ?2 WHERE id = ?1", params![id, why])?;
                tx.execute(
                    "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'ask_closed', ?4)",
                    params![ts, actor.id(), issue.key, format!("Closed with the issue, unanswered: {question}")],
                )?;
            }
        }
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
        // Its goal's last issue closing finishes the goal (NIT-611).
        if let (true, Some(goal)) = (issue.is_open() && matches!(state_type, "completed" | "canceled"), issue.goal) {
            self.complete_goal_if_finished(goal, actor)?;
        }
        self.issue(&issue.key)?.context("issue vanished")
    }

    /// Claims the most urgent Todo issue of a project for `agent`, atomically: two agents asking at
    /// once get different issues. A claim untouched for `CLAIM_TTL_SECS` counts as abandoned. An
    /// issue with an open blocker is skipped, and a blocker is as urgent as what it blocks. Among
    /// issues equally urgent, those of an active goal go first, the nearest target date first.
    pub fn next_task(&mut self, project: &str, agent: &str) -> Result<Option<IssueRecord>> {
        self.next_task_except(project, agent, &[])
    }

    /// `next_task`, passing over the issues in `except` (the crew's, for issues it cannot start now).
    pub fn next_task_except(&mut self, project: &str, agent: &str, except: &[String]) -> Result<Option<IssueRecord>> {
        self.next_task_choosing(project, agent, except, |candidates| candidates.first().map(|(k, _, _)| k.clone()))
    }

    /// `next_task_except`, letting `choose` pick among the ready candidates, given as (key, urgency
    /// rank, number), in the order `next_task` takes them (overlap::next_task prefers one clear of
    /// work in flight).
    pub fn next_task_choosing(
        &mut self,
        project: &str,
        agent: &str,
        except: &[String],
        choose: impl FnOnce(&[(String, i64, i64)]) -> Option<String>,
    ) -> Result<Option<IssueRecord>> {
        self.next_task_choosing_with_goal_budget(project, agent, except, 0, choose)
    }

    /// The shared picker with a playbook's default per-goal worker budget. Explicit goal budgets
    /// apply even without a playbook; focus and capacity are checked inside the claim transaction.
    pub fn next_task_choosing_with_goal_budget(
        &mut self,
        project: &str,
        agent: &str,
        except: &[String],
        default_goal_workers: u32,
        choose: impl FnOnce(&[(String, i64, i64)]) -> Option<String>,
    ) -> Result<Option<IssueRecord>> {
        let ts = now();
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Each candidate with whether it plans a goal (0: a whole goal waits on it, so it goes
        // ahead of other work of its urgency), its goal's place: 0 for an active goal with a target
        // date, 1 for one without, 2 for no active goal; then the target date.
        // An In Progress issue comes back only when no live claim holds it and it shows no work
        // (`idle_since`).
        // And whether it waits with an answer its asker never took (NIT-547): its latest move or
        // claim is the move back to Todo that an answer to a gone agent makes.
        let candidates: Vec<(Candidate, bool)> = {
            let mut stmt = tx.prepare(
                "SELECT i.key, i.priority, i.number,
                        CASE WHEN i.labels LIKE '%\"planning\"%' THEN 0 ELSE 1 END,
                        CASE WHEN g.id IS NULL THEN 2 WHEN g.target_date IS NULL THEN 1 ELSE 0 END, g.target_date, i.state,
                        i.state = 'Todo' AND COALESCE((SELECT e.summary FROM event e WHERE e.issue_key = i.key AND e.type IN ('issue_moved', 'claimed') ORDER BY e.id DESC LIMIT 1), '') LIKE ?3
                 FROM active_issue i LEFT JOIN goal g ON g.id = i.goal AND g.state = 'active'
                 WHERE i.project = ?1 COLLATE NOCASE AND i.origin = 'nitely' AND i.owner != 'human'
                   AND (i.state = 'Todo' OR (i.state = 'In Progress' AND (i.claimed_at IS NULL OR i.claimed_at < ?2)))",
            )?;
            let answered = format!("%{}%", crate::track::ANSWERED_WAITING_MOVE);
            let rows = stmt.query_map(params![project, ts - CLAIM_TTL_SECS, answered], |r| {
                Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?), r.get::<_, String>(6)?, r.get::<_, bool>(7)?))
            })?;
            let rows: Vec<(Candidate, String, bool)> = rows.collect::<rusqlite::Result<_>>()?;
            let sessions = recent_sessions(&tx, ts)?;
            let mut kept = Vec::new();
            for (c, state, answered) in rows {
                if state == "Todo" || idle_since(&tx, &c.0, &sessions, ts)?.is_some() {
                    kept.push((c, answered));
                }
            }
            kept
        };
        let deps = Deps::load(&tx)?;
        let mut ready: Vec<_> = candidates
            .into_iter()
            .filter(|((k, ..), _)| deps.blocked_by(k).is_empty() && !except.iter().any(|e| e.eq_ignore_ascii_case(k)))
            .map(|((k, priority, number, planning, goal, target), answered)| (pick_order(answered, rank(deps.urgency(&k, priority)), planning, goal, target, number), k))
            .collect();
        ready.sort();
        let ready: Vec<(String, i64, i64)> = ready.into_iter().map(|(o, k)| (k, o.urgency, o.number)).collect();
        let ready = crate::focus::filter_candidates(&tx, project, ready, default_goal_workers)?;
        let key = choose(&ready).filter(|k| ready.iter().any(|(r, _, _)| r == k));
        let Some(key) = key else {
            return Ok(None);
        };
        // A supervisor may have crashed before closing its worker row. Retire abandoned rows in
        // the same transaction as the new claim, so a restart cannot resume the old setup (or
        // settle an old worker) over the new owner. Leave sessions alone: the replacement may
        // already be working in the same worktree.
        let abandoned = {
            let mut stmt = tx.prepare("SELECT id, pid FROM crew_worker WHERE issue_key = ?1 AND ended_at IS NULL")?;
            let rows = stmt.query_map([&key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<u32>>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, pid) in abandoned {
            if !pid.is_some_and(crate::procs::alive) {
                tx.execute(
                    "UPDATE crew_worker SET ended_at = ?2, outcome = 'stopped', reason = 'the issue was reclaimed by another agent' WHERE id = ?1 AND ended_at IS NULL",
                    params![id, ts],
                )?;
            }
        }
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
    /// Rewrites an issue's title, body or both (NIT-210): a plan's placeholder after its design, a
    /// title that says the wrong thing. What it said before goes on its timeline as an
    /// `issue_edited` event, so nothing is lost. An issue mirrored from another tracker is changed
    /// there. Unchanged text changes nothing and writes no event.
    /// Sets an issue's priority, 0 (none) to 4 (low), as a triage after filing does (NIT-264): a
    /// board imported from another tracker with no priorities, or work whose urgency changed. The
    /// change goes on the timeline like a move ("Priority: none → High"). An issue mirrored from
    /// another tracker is changed there. The same priority again changes nothing.
    pub fn set_priority(&mut self, key: &str, priority: i64, actor: &Actor) -> Result<IssueRecord> {
        check_priority(priority)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (key, origin, old): (String, String, i64) = tx
            .query_row("SELECT key, origin, priority FROM issue WHERE key = ?1 COLLATE NOCASE", [key.trim()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
            .with_context(|| format!("no issue {key}"))?;
        if origin != "nitely" {
            bail!("{key} is mirrored from {origin}; change it there");
        }
        if old != priority {
            let name = |p: i64| PRIORITY_NAMES.get(p as usize).copied().unwrap_or("none");
            let ts = now();
            tx.execute("UPDATE issue SET priority = ?2, updated_at = ?3 WHERE key = ?1", params![key, priority, iso(ts)])?;
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'priority_changed', ?4)",
                params![ts, actor.id(), key, format!("Priority: {} → {}", name(old), name(priority))],
            )?;
        }
        tx.commit()?;
        self.issue(&key)?.with_context(|| format!("no issue {key}"))
    }

    /// Preflight a compound edit before its other fields are written. The setter repeats this
    /// check under its write lock, so callers cannot use a stale preflight to lower the level.
    pub fn check_review_level_change(&self, key: &str, level: Option<&str>, actor: &Actor) -> Result<()> {
        let level = level.map(str::trim).filter(|s| !s.is_empty() && *s != "undeclared");
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.origin != "nitely" {
            bail!("{} is mirrored from {}; change it there", issue.key, issue.origin);
        }
        check_review_change(issue.review_level.as_deref(), level, actor)
    }

    /// Sets the declared review level, or clears it (`level` None or "undeclared"). An agent can
    /// only raise it. The person can set any level or clear it. An agent that holds the issue
    /// (its claim) is the implementer: the stored source is `implementer`, a hint. Anyone else
    /// acting as an agent is a coordinator.
    pub fn set_review_level(&mut self, key: &str, level: Option<&str>, note: Option<&str>, actor: &Actor) -> Result<IssueRecord> {
        let level = level.map(str::trim).filter(|s| !s.is_empty() && *s != "undeclared");
        if let Some(l) = level {
            check_review_level(l)?;
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT key, origin, review_level, claimed_by FROM issue WHERE key = ?1 COLLATE NOCASE",
                [key.trim()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (key, origin, old, claimed_by) = row.with_context(|| format!("no issue {}", key.trim()))?;
        if origin != "nitely" {
            bail!("{key} is mirrored from {origin}; change it there");
        }
        let source = match actor {
            Actor::Human => "human",
            Actor::Agent(id) if claimed_by.as_deref() == Some(id.as_str()) => "implementer",
            Actor::Agent(_) => "coordinator",
        };
        check_review_change(old.as_deref(), level, actor)?;
        if old.as_deref() != level {
            let ts = now();
            tx.execute(
                "UPDATE issue SET review_level = ?2, review_level_by = ?3, review_level_source = ?4, review_level_at = ?5, updated_at = ?6 WHERE key = ?1",
                params![key, level, actor.id(), if level.is_some() { Some(source) } else { None::<&str> }, ts, iso(ts)],
            )?;
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'review_level_changed', ?4)",
                params![ts, actor.id(), key, review_change_summary(old.as_deref(), level, source, note)],
            )?;
        }
        tx.commit()?;
        self.issue(&key)?.with_context(|| format!("no issue {key}"))
    }

    pub fn edit_issue(&mut self, key: &str, title: Option<&str>, body: Option<&str>, actor: &Actor) -> Result<IssueRecord> {
        let title = title.map(str::trim);
        let body = body.map(str::trim);
        if title.is_some_and(str::is_empty) {
            bail!("an issue needs a title");
        }
        if body.is_some_and(str::is_empty) {
            bail!("an issue needs a body: what is wrong or wanted, and why");
        }
        // The text it had is read under the write lock: two edits at once each keep the text the
        // other replaced, and none is lost.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (key, origin, old_title, old_body): (String, String, String, String) = tx
            .query_row("SELECT key, origin, title, COALESCE(body, '') FROM issue WHERE key = ?1 COLLATE NOCASE", [key.trim()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?
            .with_context(|| format!("no issue {key}"))?;
        if origin != "nitely" {
            bail!("{key} is mirrored from {origin}; change it there");
        }
        let title = title.filter(|t| *t != old_title);
        let body = body.filter(|b| *b != old_body);
        if title.is_some() || body.is_some() {
            let mut was = Vec::new();
            if title.is_some() {
                was.push(format!("Title was: {old_title}"));
            }
            if body.is_some() {
                was.push(format!("Body was:\n{old_body}"));
            }
            let ts = now();
            tx.execute(
                "UPDATE issue SET title = COALESCE(?2, title), body = COALESCE(?3, body), updated_at = ?4 WHERE key = ?1",
                params![key, title, body, iso(ts)],
            )?;
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'issue_edited', ?4)",
                params![ts, actor.id(), key, was.join("\n\n")],
            )?;
        }
        tx.commit()?;
        self.issue(&key)?.with_context(|| format!("no issue {key}"))
    }

    pub fn set_owner(&mut self, key: &str, owner: &str, actor: &Actor) -> Result<IssueRecord> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.origin != "nitely" {
            bail!("{} is mirrored from {}; change it there", issue.key, issue.origin);
        }
        let owner = owner.trim();
        self.check_owner_change(key, owner)?;
        self.check_plan_owner_change(key, owner)?;
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

    /// Keeps a claim held by `agent` fresh, as a note from it does. An agent whose calls now act as
    /// its session (NIT-194) still holds the claims it took under its folder's id before.
    pub fn renew_claim(&self, key: &str, agent: &str) -> Result<()> {
        self.conn.execute("UPDATE issue SET claimed_at = ?3 WHERE key = ?1 COLLATE NOCASE AND claimed_by = ?2", params![key, agent, now()])?;
        Ok(())
    }

    /// What starting an issue's task does to it (NIT-274): `agent` holds it, as `next_task`'s claim
    /// does, and one not started yet moves to In Progress, so a second agent asking `next_task` is
    /// not handed work in progress. Agents are mostly given issues by name, and before this only
    /// `next_task` claimed. A claim of `agent`'s own (or of `mine`, the ids it acted under before)
    /// is renewed. Another agent's claim is refused while that agent is live: its session open and
    /// seen within `GONE_AFTER_SECS`, or its claim renewed within the claim TTL; one whose session
    /// ended, or that went quiet past both, is taken over, and so is any with `take_over`. Returns
    /// what the caller should be told: a claim it took over.
    pub fn claim_for_start(&self, key: &str, agent: &str, mine: &[String], take_over: bool) -> Result<Option<String>> {
        let ts = now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let archived: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM issue i JOIN project p ON p.key = i.project WHERE i.key = ?1 COLLATE NOCASE AND p.archived = 1)", [key], |r| r.get(0))?;
        if archived { bail!("this project is archived; restore it in Settings before starting new work"); }
        let (key, state, claimed_by, claimed_at, origin, owner): StartRow = tx
            .query_row("SELECT key, state, claimed_by, claimed_at, origin, owner FROM issue WHERE key = ?1 COLLATE NOCASE", [key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .optional()?
            .with_context(|| format!("no issue {key}"))?;
        // A mirrored issue's state is its tracker's, and the human's own work is not an agent's.
        if origin != "nitely" || owner == "human" {
            return Ok(None);
        }
        let mut told = None;
        let session_of = |id: &str| -> Result<Option<(String, i64)>> {
            Ok(tx.query_row("SELECT state, last_seen_at FROM session WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
        };
        let seen_live = |s: &Option<(String, i64)>| s.as_ref().is_some_and(|(state, seen)| state != "ended" && *seen >= ts - crate::sessions::GONE_AFTER_SECS);
        // An id of `mine` is the caller's own only while no live session goes by it: a crew
        // worker's reports go under its folder's id, and another session in that folder is not it.
        let own = |h: &String| -> Result<bool> { Ok(h == agent || (mine.contains(h) && !seen_live(&session_of(h)?))) };
        // A claim of the caller's own under `<vendor>:<path>` (the crew's, for its worker) keeps its
        // id, which the holder's reports renew. One under a bare vendor id (`nitely issue next
        // --agent claude`) moves to the caller's: nothing would renew it, and it would lapse while
        // the agent still works (NIT-367).
        let (other, own_claim) = match claimed_by.clone() {
            Some(h) if !own(&h)? => (Some(h), None),
            Some(h) => (None, Some(h).filter(|h| h.contains(':'))),
            None => (None, None),
        };
        if let Some(holder) = other {
            let session = session_of(&holder)?;
            let fresh = claimed_at.is_some_and(|c| c >= ts - CLAIM_TTL_SECS);
            let live = match &session {
                Some((state, seen)) => state != "ended" && (*seen >= ts - crate::sessions::GONE_AFTER_SECS || fresh),
                None => fresh,
            };
            if live && !take_over {
                bail!(
                    "{key} is claimed by {holder}, which is still on it; take other work (next_task), or start it with take_over only if you know that agent stopped"
                );
            }
            let why = if live { "take_over" } else if session.is_some_and(|(s, _)| s == "ended") { "its session ended" } else { "it went quiet" };
            told = Some(format!("took over {key} from {holder} ({why})"));
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'claimed', ?4)",
                params![ts, agent, key, format!("Took the issue over from {holder} ({why})")],
            )?;
        } else if claimed_by.is_none() {
            tx.execute("INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'claimed', 'Took the issue')", params![ts, agent, key])?;
        }
        // A claim of the caller's own is renewed under the id it has: the crew renews its worker's
        // by that id, whoever starts its worktree.
        tx.execute("UPDATE issue SET claimed_by = COALESCE(?4, ?2), claimed_at = ?3 WHERE key = ?1", params![key, agent, ts, own_claim])?;
        if state == "Todo" || state == "Backlog" {
            tx.execute("UPDATE issue SET state = 'In Progress', state_type = 'started', updated_at = ?2 WHERE key = ?1", params![key, iso(ts)])?;
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'issue_moved', ?4)",
                params![ts, agent, key, format!("{state} → In Progress")],
            )?;
        }
        tx.commit()?;
        Ok(told)
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
            "SELECT issue_key, ts, actor, type, summary FROM event
             WHERE issue_key COLLATE NOCASE IN (SELECT value FROM json_each(?1)) ORDER BY ts, id",
        )?;
        let rows = stmt.query_map([serde_json::to_string(&open)?], |r| {
            Ok((r.get::<_, String>(0)?, TimelineEntry { ts: r.get(1)?, actor: r.get(2)?, kind: r.get(3)?, summary: r.get(4)?, place: None }))
        })?;
        for row in rows {
            let (key, entry) = row?;
            let key = key.to_uppercase();
            if open.contains(&key) {
                timelines.entry(key).or_default().push(entry);
            }
        }
        let changes = self.change_reviews()?;
        let ts = now();
        let mut sessions = None;
        for i in issues.iter_mut() {
            let key = i.key.to_uppercase();
            let timeline = if i.is_open() { timelines.get(&key).map(Vec::as_slice).unwrap_or(&[]) } else { &[] };
            i.track = track_of(i, timeline, changes.get(&key).copied().unwrap_or(0));
            if i.track.stuck.is_none() && i.state == "In Progress" {
                if sessions.is_none() {
                    sessions = Some(recent_sessions(&self.conn, ts)?);
                }
                if let Some(last) = idle_since(&self.conn, &i.key, sessions.as_deref().unwrap_or_default(), ts)? {
                    i.track.stuck = Some(idle_stuck(last, ts));
                }
            }
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
            verification: crate::verification::recent(self,&issue.key)?,
            relations: self.relations(&issue.key)?,
            timeline: self.timeline(&issue.key)?,
            prs: self.prs_for_issue(&issue.key)?,
            reviews: self.reviews(&issue.key)?,
            proposal: self.proposal_for_issue(&issue.key)?,
            goal_plan: self.goal_plan_for_issue(&issue.key)?,
            // A database a newer schema has not reached yet (the read-only preview) shows none.
            batches: self.issue_batches(&issue.key).unwrap_or_default(),
            asks: self.asks_about(&issue.key)?,
            issue,
        }))
    }

    /// Every ask about issue `key`, oldest first.
    pub fn asks_about(&self, key: &str) -> Result<Vec<Ask>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ASK_COLUMNS} FROM ask a LEFT JOIN issue i ON i.key = a.issue_key COLLATE NOCASE WHERE a.issue_key = ?1 COLLATE NOCASE ORDER BY a.id"
        ))?;
        let rows = stmt.query_map([key], ask_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn timeline(&self, key: &str) -> Result<Vec<TimelineEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.ts, e.actor, e.type, e.summary, COALESCE(s.cwd, e.cwd) FROM event e LEFT JOIN session s ON s.id = e.actor
             WHERE e.issue_key = ?1 COLLATE NOCASE ORDER BY e.ts, e.id",
        )?;
        let rows = stmt.query_map([key], |r| {
            Ok(TimelineEntry { ts: r.get(0)?, actor: r.get(1)?, kind: r.get(2)?, summary: r.get(3)?, place: r.get(4)? })
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
                &format!("SELECT {ASK_COLUMNS} FROM ask a LEFT JOIN issue i ON i.key = a.issue_key COLLATE NOCASE WHERE a.id = ?1"),
                [id],
                ask_from_row,
            )
            .optional()?)
    }

    /// The questions waiting on the person. One whose issue has closed is not: its issue's close
    /// closed it (and one asked before that rule, on an issue closed since, is passed over too).
    pub fn open_asks(&self) -> Result<Vec<Ask>> {
        self.read_open_asks(false)
    }

    /// A hidden question still participates in acceptance and review safety rules.
    pub(crate) fn all_open_asks(&self) -> Result<Vec<Ask>> {
        self.read_open_asks(true)
    }

    fn read_open_asks(&self, include_archived: bool) -> Result<Vec<Ask>> {
        let table = if include_archived { "ask" } else { "active_ask" };
        let mut stmt = self.conn.prepare(
            &format!(
                "SELECT {ASK_COLUMNS} FROM {table} a LEFT JOIN issue i ON i.key = a.issue_key COLLATE NOCASE
                 WHERE a.answer IS NULL AND a.closed IS NULL
                   AND (i.key IS NULL OR i.state_type NOT IN ('completed', 'canceled'))
                 ORDER BY a.asked_at"
            ),
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
        if let Some(why) = &ask.closed {
            bail!("ask {id} needs no answer any more: it {why}");
        }
        let ts = now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE ask SET answer = ?2, answered_at = ?3 WHERE id = ?1",
            params![id, answer.trim(), ts],
        )?;
        if ask.issue_key.is_none() {
            // An ask about no issue: its answer is still news to its asker (the updates feed,
            // NIT-562), so the event says whose ask it was.
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, 'human', NULL, 'answer', ?2, ?3)",
                params![ts, answer.trim(), serde_json::json!({ "ask": id, "asked_by": ask.asked_by }).to_string()],
            )?;
        }
        if let Some(k) = &ask.issue_key {
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', ?2, 'answer', ?3)",
                params![ts, k, answer.trim()],
            )?;
            // The work that waited on it goes on (NIT-277): an issue `ask` put in Needs You goes
            // back, once nothing else it asked is open, to its asker while that agent is live, and
            // otherwise to Todo, where next_task gives it to the next agent with the answer on its
            // timeline.
            // Whether the Needs You it is in is an ask's wait: the move that began that wait, recorded
            // on its asks, is still the issue's latest entry into Needs You. An answer to any question
            // in that wait (a triage question that went to the person, say) can then end it; a Needs
            // You entered since, for an approval, is not the ask's to end. It goes back to the agent
            // whose question began the wait.
            let entry = latest_needs_you_entry(&tx, k)?;
            // A review that needs the person's approval, recorded since the wait began, is an
            // approval waiting in the same Needs You (submit moves it there without leaving it
            // first): the answer does not end that, nor one the person's acceptance of a finding
            // left waiting (`awaits_approval`). A review that asked for changes sends the work back
            // to its agent, so it does not count.
            let moved: Option<String> = match entry {
                Some(e) => tx
                    .query_row(
                        "SELECT a.asked_by FROM ask a JOIN event w ON w.id = ?2
                         WHERE a.issue_key = ?1 AND a.wait_event = ?2 AND a.moved_from IS NOT NULL
                           AND NOT EXISTS (SELECT 1 FROM review r WHERE r.issue_key = ?1 COLLATE NOCASE AND r.approver = 'human' AND r.verdict != 'changes' AND r.ts >= w.ts)
                           AND NOT EXISTS (SELECT 1 FROM event x WHERE x.issue_key = ?1 AND x.type = 'awaits_approval' AND x.id > w.id)
                         ORDER BY a.id LIMIT 1",
                        params![k, e],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => None,
            };
            let state: Option<String> = tx.query_row("SELECT state FROM issue WHERE key = ?1", [k], |r| r.get(0)).optional()?;
            // Any other question on it still open keeps it waiting, whoever asked it; one about a
            // review finding holds only the land (`keeps_waiting`).
            let open: Vec<bool> = tx
                .prepare("SELECT finding IS NOT NULL FROM ask WHERE issue_key = ?1 AND answer IS NULL AND closed IS NULL")?
                .query_map([k], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            if let Some(asker) = moved.filter(|_| state.as_deref() == Some(NEEDS_HUMAN) && !keeps_waiting(&open)) {
                let seen: Option<(String, i64)> =
                    tx.query_row("SELECT state, last_seen_at FROM session WHERE id = ?1", [&asker], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                // An asker with no live session record (gone, or one that cannot be told: the CLI,
                // a remote agent) gets the answer as a notice if it calls again, and the work waits
                // in Todo for whoever claims it next.
                let (to, to_type, claim, why) = match seen {
                    Some((s, at)) if s != "ended" && at >= ts - crate::sessions::GONE_AFTER_SECS => {
                        ("In Progress", "started", true, "answered; back to the agent that asked")
                    }
                    _ => ("Todo", "unstarted", false, "answered; it waits for the next agent, with the answer on its timeline"),
                };
                tx.execute(
                    "UPDATE issue SET state = ?2, state_type = ?3, updated_at = ?4,
                       claimed_by = CASE WHEN ?5 THEN ?6 ELSE NULL END, claimed_at = CASE WHEN ?5 THEN ?7 ELSE NULL END
                     WHERE key = ?1",
                    params![k, to, to_type, iso(ts), claim, asker, ts],
                )?;
                tx.execute(
                    "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', ?2, 'issue_moved', ?3)",
                    params![ts, k, format!("{NEEDS_HUMAN} → {to}: {why}")],
                )?;
            }
        }
        tx.commit()?;
        self.ask(id)?.context("ask vanished")
    }

    /// What `ask` does to the issue it is about (NIT-277): one In Progress waits in Needs You for
    /// the answer, and the ask notes where it was, so the answer can put it back (`answer_ask`).
    pub fn wait_on_ask(&mut self, ask: &Ask, actor: &Actor) -> Result<()> {
        let Some(key) = &ask.issue_key else { return Ok(()) };
        let Some(issue) = self.issue(key)? else { return Ok(()) };
        if issue.origin != "nitely" || ask.finding.is_some() {
            return Ok(());
        }
        // Another question while it already waits on an ask's answer joins that wait: whichever is
        // answered last puts it back.
        if issue.state == NEEDS_HUMAN {
            if let Some(entry) = latest_needs_you_entry(&self.conn, key)? {
                self.conn.execute(
                    "UPDATE ask SET wait_event = ?3,
                       moved_from = (SELECT moved_from FROM ask WHERE issue_key = ?2 AND wait_event = ?3 AND moved_from IS NOT NULL ORDER BY id LIMIT 1)
                     WHERE id = ?1 AND EXISTS (SELECT 1 FROM ask WHERE issue_key = ?2 AND wait_event = ?3 AND moved_from IS NOT NULL)",
                    params![ask.id, key, entry],
                )?;
            }
            return Ok(());
        }
        if issue.state != "In Progress" {
            return Ok(());
        }
        self.move_issue(key, NEEDS_HUMAN, None, Some(&format!("waiting for the answer to ask {}", ask.id)), actor)?;
        let entry = latest_needs_you_entry(&self.conn, key)?;
        self.conn.execute("UPDATE ask SET moved_from = 'In Progress', wait_event = ?2 WHERE id = ?1", params![ask.id, entry])?;
        Ok(())
    }

    /// Answers to asks of `askers` (an agent's ids) not told to it yet, oldest first: each comes to
    /// the agent once, as a notice on its next nitely call (NIT-277).
    pub fn undelivered_answers(&self, askers: &[&str]) -> Result<Vec<Ask>> {
        let mut out = Vec::new();
        for asker in askers {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {ASK_COLUMNS} FROM ask a LEFT JOIN issue i ON i.key = a.issue_key COLLATE NOCASE
                 WHERE a.asked_by = ?1 AND a.answer IS NOT NULL AND a.delivered_at IS NULL ORDER BY a.answered_at"
            ))?;
            let rows = stmt.query_map([asker], ask_from_row)?;
            for a in rows {
                let a = a?;
                if !out.iter().any(|o: &Ask| o.id == a.id) {
                    out.push(a);
                }
            }
        }
        Ok(out)
    }

    /// Notes that these asks' answers reached the agents that asked.
    pub fn mark_answers_delivered(&self, ids: &[i64], to: &str) -> Result<()> {
        let ts = now();
        for id in ids {
            self.conn.execute("UPDATE ask SET delivered_at = ?2, delivered_to = ?3 WHERE id = ?1 AND delivered_at IS NULL", params![id, ts, to])?;
        }
        Ok(())
    }

    /// Answers on issue `key` that reached no agent yet (NIT-547): what the agent that takes the
    /// issue up next is told when its asker has gone.
    pub fn answers_waiting_on(&self, key: &str) -> Result<Vec<Ask>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ASK_COLUMNS} FROM ask a LEFT JOIN issue i ON i.key = a.issue_key COLLATE NOCASE
             WHERE a.issue_key = ?1 COLLATE NOCASE AND a.answer IS NOT NULL AND a.delivered_at IS NULL ORDER BY a.answered_at"
        ))?;
        let rows = stmt.query_map([key], ask_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// The event of the issue's latest move into Needs You from another state: where its current wait
/// began. A move from Needs You to Needs You begins none.
fn latest_needs_you_entry(conn: &rusqlite::Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MAX(id) FROM event WHERE issue_key = ?1 AND type = 'issue_moved'
           AND (summary LIKE '% → Needs You' OR summary LIKE '% → Needs You:%') AND summary NOT LIKE 'Needs You →%'",
        [key],
        |r| r.get(0),
    )?)
}

/// An ask's columns, read joined to its issue as `i`. An unanswered ask whose issue closed before
/// closing an issue closed its asks (NIT-181) reads as closed with it, as one closed since does.
const ASK_COLUMNS: &str = "a.id, a.issue_key, a.asked_by, a.question, a.options, a.answer, a.asked_at, a.answered_at,
    COALESCE(a.closed, CASE WHEN a.answer IS NULL AND i.state_type IN ('completed', 'canceled')
                            THEN 'closed with ' || i.key || ' (' || i.state || ')' END), a.finding, a.delivered_at, a.delivered_to";

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
        closed: r.get(8)?,
        finding: r.get::<_, Option<String>>(9)?.and_then(|f| serde_json::from_str(&f).ok()),
        delivered_at: r.get(10)?,
        delivered_to: r.get(11)?,
    })
}

/// Whether an issue's open questions keep it waiting for the person, given for each whether it is
/// a request to accept a review finding: any other question does. A finding's question holds only
/// the land, never the agent (NIT-568, NIT-574).
pub(crate) fn keeps_waiting(open_is_finding: &[bool]) -> bool {
    open_is_finding.iter().any(|finding| !finding)
}

#[cfg(test)]
mod tests {

    /// NIT-574: an open finding question does not keep an answered issue waiting.
    #[test]
    fn only_a_question_that_is_not_about_a_finding_keeps_the_issue_waiting() {
        assert!(!keeps_waiting(&[]), "nothing else open");
        assert!(!keeps_waiting(&[true, true]), "finding questions hold the land, not the agent");
        assert!(keeps_waiting(&[true, false]), "another ordinary question still waits");
    }
    use super::*;

    /// next_task's order (NIT-547): an issue answered while its agent was gone goes first, whatever
    /// its priority; the rest by urgency, then planning, goal place, target date and number.
    #[test]
    fn an_answer_waiting_for_an_agent_is_picked_first() {
        let mut order = vec![
            (pick_order(false, 1, 1, 2, None, 3), "urgent"),
            (pick_order(true, 4, 1, 2, None, 9), "answered, low priority"),
            (pick_order(false, 1, 1, 0, Some("2026-11-01".into()), 5), "urgent in a dated goal"),
            (pick_order(false, 3, 0, 2, None, 1), "a planning issue"),
        ];
        order.sort();
        let picked: Vec<&str> = order.iter().map(|(_, n)| *n).collect();
        assert_eq!(picked, ["answered, low priority", "urgent in a dated goal", "urgent", "a planning issue"]);
    }

    fn db() -> (Db, std::path::PathBuf) {
        let dir = crate::paths::unique_temp("nitely-issues", "");
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
    fn a_timeline_entry_says_which_session_wrote_it_and_where_it_works() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let session = Actor::Agent("claude:60f2a6c3-5858-4141-b8bb-a52c02f13317".into());
        db.record(&crate::db::NewEvent { session_id: session.id().into(), vendor: "claude".into(), cwd: r"C:\Code\my-app".into(), kind: "session_start".into(), summary: None, state: "working".into() }).unwrap();
        db.create_issue(&new("One"), &session).unwrap();
        db.add_note("NIT-1", "picked it up", "note", &Actor::Agent("claude:c:/code/my-app".into())).unwrap();
        let timeline = db.timeline("NIT-1").unwrap();
        assert_eq!((timeline[0].actor.as_str(), timeline[0].place.as_deref()), (session.id(), Some(r"C:\Code\my-app")));
        let note = timeline.iter().find(|entry| entry.kind == "note").unwrap();
        assert_eq!((note.actor.as_str(), note.place.as_deref()), ("claude:c:/code/my-app", None), "an older folder id says its folder itself");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_priority_set_after_filing_goes_on_the_timeline() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:x".into());
        db.create_issue(&NewIssue { priority: 0, ..new("Imported with no priority") }, &agent).unwrap();
        assert_eq!(db.set_priority("nit-1", 2, &agent).unwrap().priority, 2);
        let last = db.timeline("NIT-1").unwrap().pop().unwrap();
        assert_eq!((last.kind.as_str(), last.summary.as_deref()), ("priority_changed", Some("Priority: none → High")));
        let events = db.timeline("NIT-1").unwrap().len();
        db.set_priority("NIT-1", 2, &agent).unwrap();
        assert_eq!(db.timeline("NIT-1").unwrap().len(), events, "the same priority: no event");
        db.set_priority("NIT-1", 0, &agent).unwrap();
        assert_eq!(db.timeline("NIT-1").unwrap().pop().unwrap().summary.as_deref(), Some("Priority: High → none"));
        assert!(db.set_priority("NIT-1", 5, &agent).is_err() && db.set_priority("NIT-1", -1, &agent).is_err());
        db.conn.execute("INSERT INTO issue (key, origin, title, state, state_type, updated_at) VALUES ('LIN-1', 'linear', 'Mirrored', 'Todo', 'unstarted', '')", []).unwrap();
        assert!(db.set_priority("LIN-1", 1, &agent).unwrap_err().to_string().contains("mirrored from linear"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn new_review_declarations_default_to_standard_without_rewriting_legacy_rows() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        db.conn.execute("INSERT INTO issue (key, origin, title, state, state_type, updated_at) VALUES ('OLD-1', 'nitely', 'Legacy', 'Todo', 'unstarted', '')", []).unwrap();
        let filed = db.create_issue(&new("Default review declaration"), &Actor::Agent("codex:test".into())).unwrap();
        assert_eq!(filed.review_level.as_deref(), Some("standard"));
        assert_eq!(filed.review_level_by.as_deref(), Some("codex:test"));
        assert_eq!(filed.review_level_source.as_deref(), Some("author"));
        assert!(db.issue("OLD-1").unwrap().unwrap().review_level.is_none());
        assert!(db.check_review_level_change(&filed.key, Some("inert"), &Actor::Agent("cli".into())).is_err());
        assert!(db.check_review_level_change(&filed.key, Some("inert"), &Actor::Human).is_ok());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_review_level_is_declared_at_filing_and_only_the_person_lowers_it() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:x".into());
        let filed = db
            .create_issue(&NewIssue { review_level: Some("inert".into()), ..new("Fix the empty-state sentence") }, &agent)
            .unwrap();
        assert_eq!(filed.review_level.as_deref(), Some("inert"));
        assert_eq!(filed.review_level_source.as_deref(), Some("author"));
        assert_eq!(filed.review_level_by.as_deref(), Some("claude:x"));
        let declared = db.timeline("NIT-1").unwrap().pop().unwrap();
        assert_eq!(declared.kind, "review_level_changed");
        assert_eq!(declared.summary.as_deref(), Some("Review level: undeclared → Basic (inert) (author)"));
        let (level, note) = resolve_review_level(Some("inert"), Some("billing (src/pay.ts)")).unwrap();
        assert_eq!(level.as_deref(), Some("sensitive"));
        assert_eq!(note.as_deref(), Some("inert raised: billing (src/pay.ts)"));
        assert!(resolve_review_level(Some("nope"), None).is_err());
        db.conn.execute("UPDATE issue SET claimed_by = 'claude:x' WHERE key = 'NIT-1'", []).unwrap();
        let hint = db.set_review_level("NIT-1", Some("contained"), None, &agent).unwrap();
        assert_eq!(hint.review_level_source.as_deref(), Some("implementer"));
        assert!(db.timeline("NIT-1").unwrap().pop().unwrap().summary.unwrap().contains("a hint"));
        let err = db.set_review_level("NIT-1", Some("inert"), None, &agent).unwrap_err().to_string();
        assert!(err.contains("only be raised"), "{err}");
        assert!(db.set_review_level("NIT-1", None, None, &agent).is_err());
        let other = Actor::Agent("codex:y".into());
        let raised = db.set_review_level("NIT-1", Some("standard"), None, &other).unwrap();
        assert_eq!((raised.review_level.as_deref(), raised.review_level_source.as_deref()), (Some("standard"), Some("coordinator")));
        let cleared = db.set_review_level("NIT-1", None, None, &Actor::Human).unwrap();
        assert_eq!(cleared.review_level, None);
        let again = db.set_review_level("nit-1", Some("sensitive"), None, &Actor::Human).unwrap();
        assert_eq!(again.review_level.as_deref(), Some("sensitive"));
        let events = db.timeline("NIT-1").unwrap().len();
        db.set_review_level("NIT-1", Some("sensitive"), None, &Actor::Human).unwrap();
        assert_eq!(db.timeline("NIT-1").unwrap().len(), events, "the same level: no event");
        db.conn.execute("INSERT INTO issue (key, origin, title, state, state_type, updated_at) VALUES ('LIN-1', 'linear', 'Mirrored', 'Todo', 'unstarted', '')", []).unwrap();
        assert!(db.set_review_level("LIN-1", Some("inert"), None, &Actor::Human).unwrap_err().to_string().contains("mirrored"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_edit_keeps_what_the_issue_said_before() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:x".into());
        db.create_issue(&new("Slice 2: placeholder"), &agent).unwrap();
        let edited = db.edit_issue("nit-1", Some("Slice 2: the export dialog"), Some("Export the list as CSV."), &agent).unwrap();
        assert_eq!((edited.title.as_str(), edited.body.as_str()), ("Slice 2: the export dialog", "Export the list as CSV."));
        let last = db.timeline("NIT-1").unwrap().pop().unwrap();
        assert_eq!(last.kind, "issue_edited");
        assert_eq!(last.summary.as_deref(), Some("Title was: Slice 2: placeholder\n\nBody was:\nwhy"));

        // Only what changed is kept; the same text again changes nothing.
        db.edit_issue("NIT-1", None, Some("Export the list as CSV or JSON."), &agent).unwrap();
        assert_eq!(db.timeline("NIT-1").unwrap().pop().unwrap().summary.as_deref(), Some("Body was:\nExport the list as CSV."));
        let events = db.timeline("NIT-1").unwrap().len();
        db.edit_issue("NIT-1", Some("Slice 2: the export dialog "), None, &agent).unwrap();
        assert_eq!(db.timeline("NIT-1").unwrap().len(), events, "unchanged: no event");

        // Two writers on the same database: each edit keeps the text the one before it replaced.
        let mut other = Db::open(&dir.join("t.db")).unwrap();
        other.edit_issue("NIT-1", Some("Slice 2: export"), None, &agent).unwrap();
        db.edit_issue("NIT-1", Some("Slice 2: export as CSV"), None, &agent).unwrap();
        let titles: Vec<String> = db.timeline("NIT-1").unwrap().iter().rev().take(2).filter_map(|t| t.summary.clone()).collect();
        assert_eq!(titles, ["Title was: Slice 2: export", "Title was: Slice 2: the export dialog"]);
        drop(other);

        assert!(db.edit_issue("NIT-1", Some("  "), None, &agent).is_err(), "no empty title");
        assert!(db.edit_issue("NIT-1", None, Some(""), &agent).is_err(), "no empty body");
        assert!(db.edit_issue("NIT-9", Some("x"), None, &agent).is_err());
        db.conn.execute("INSERT INTO issue (key, origin, title, state, state_type, updated_at) VALUES ('LIN-1', 'linear', 'Mirrored', 'Todo', 'unstarted', '')", []).unwrap();
        let err = db.edit_issue("LIN-1", Some("Mine now"), None, &agent).unwrap_err().to_string();
        assert!(err.contains("mirrored from linear"), "{err}");
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
        assert_eq!(db.timeline("NIT-1").unwrap().len(), 4, "filing, default review declaration and two moves");
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

    /// start_task's claim (NIT-274): the agent starting an issue holds it and moves it to In
    /// Progress, so next_task passes it over. Another agent's start is refused while the holder is
    /// live, and takes it over once the holder's session ended or with take_over.
    #[test]
    fn starting_an_issue_claims_it_and_refuses_another_live_agent() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        db.create_issue(&new("Given by name"), &Actor::Human).unwrap();
        assert_eq!(db.claim_for_start("NIT-1", "claude:c:/code/app", &[], false).unwrap(), None);
        let i = db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((i.state.as_str(), i.claimed_by.as_deref()), ("In Progress", Some("claude:c:/code/app")));
        assert!(db.next_task("NIT", "codex:b").unwrap().is_none(), "not handed to another agent");

        // The same agent, now acting as its session, holds what it claimed under its folder's id.
        db.conn
            .execute("INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('claude:s', 'claude', 'C:/code/app', ?1, ?1, 'working')", [now()])
            .unwrap();
        assert_eq!(db.claim_for_start("NIT-1", "claude:s", &["claude:c:/code/app".into()], false).unwrap(), None);
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().claimed_by.as_deref(), Some("claude:c:/code/app"), "renewed under its id");
        db.conn.execute("UPDATE issue SET claimed_by = 'claude:s' WHERE key = 'NIT-1'", []).unwrap();

        // Another agent, while that session is live: refused.
        let err = db.claim_for_start("NIT-1", "codex:b", &[], false).unwrap_err().to_string();
        assert!(err.contains("claimed by claude:s") && err.contains("take_over"), "{err}");
        // Once the session ended, taken over and said so.
        db.conn.execute("UPDATE session SET state = 'ended'", []).unwrap();
        let took = db.claim_for_start("NIT-1", "codex:b", &[], false).unwrap().unwrap();
        assert!(took.contains("from claude:s (its session ended)"), "{took}");
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().claimed_by.as_deref(), Some("codex:b"));

        // A holder with no session record counts as live while its claim is fresh; take_over takes it.
        assert!(db.claim_for_start("NIT-1", "cursor:c", &[], false).is_err());
        assert!(db.claim_for_start("NIT-1", "cursor:c", &[], true).unwrap().unwrap().contains("take_over"));
        // A mirrored issue is left to its tracker.
        db.conn.execute("INSERT INTO issue (key, origin, title, state, state_type, updated_at) VALUES ('LIN-1', 'linear', 'Mirrored', 'Todo', 'unstarted', '')", []).unwrap();
        db.claim_for_start("LIN-1", "codex:b", &[], false).unwrap();
        assert_eq!(db.issue("LIN-1").unwrap().unwrap().state, "Todo");

        // Another session in a crew worker's folder is not the worker, whose reports keep a live
        // session under the folder's id: refused, though it shares that id.
        db.create_issue(&new("A crew worker's"), &Actor::Human).unwrap();
        db.claim_for_start("NIT-2", "claude:c:/wt/w1", &[], false).unwrap();
        db.conn
            .execute("INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('claude:c:/wt/w1', 'claude', 'C:/wt/w1', ?1, ?1, 'working')", [now()])
            .unwrap();
        assert!(db.claim_for_start("NIT-2", "claude:other", &["claude:c:/wt/w1".into()], false).is_err());

        // A crew worker's claim, made for its worktree before it runs, is the start's own when the
        // start names that id among its own; a bare-vendor claim (`issue next --agent codex`) too.
        db.create_issue(&new("Started by the crew"), &Actor::Human).unwrap();
        db.conn.execute("UPDATE issue SET claimed_by = 'claude:c:/wt/w2', claimed_at = ?1 WHERE key = 'NIT-3'", [now()]).unwrap();
        assert!(db.claim_for_start("NIT-3", "claude:c:/code/app", &[], false).is_err(), "not someone else's");
        db.claim_for_start("NIT-3", "claude:c:/code/app", &["claude:c:/wt/w2".into(), "claude".into()], false).unwrap();
        let i = db.issue("NIT-3").unwrap().unwrap();
        assert_eq!((i.state.as_str(), i.claimed_by.as_deref()), ("In Progress", Some("claude:c:/wt/w2")), "the worker's claim stays the worker's");

        // A claim under a bare vendor id (`issue next --agent claude`) moves to the starter's id,
        // which its reports renew (NIT-367).
        db.create_issue(&new("Taken with issue next"), &Actor::Human).unwrap();
        db.conn.execute("UPDATE issue SET claimed_by = 'claude', claimed_at = ?1, state = 'In Progress', state_type = 'started' WHERE key = 'NIT-4'", [now()]).unwrap();
        db.claim_for_start("NIT-4", "claude:c:/code/app", &["claude".into()], false).unwrap();
        assert_eq!(db.issue("NIT-4").unwrap().unwrap().claimed_by.as_deref(), Some("claude:c:/code/app"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Work started with start_task holds no claim. An In Progress issue that no live claim holds
    /// is offered again, and shows stuck, only once nothing has happened on it and no session has
    /// worked in its worktree for the claim TTL (NIT-275).
    #[test]
    fn an_unclaimed_in_progress_issue_comes_back_only_once_it_shows_no_work() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:a".into());
        db.create_issue(&new("Started by hand"), &agent).unwrap();
        db.move_issue("NIT-1", "In Progress", None, None, &agent).unwrap();
        assert!(db.next_task("NIT", "claude:b").unwrap().is_none(), "just started");
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().track.stuck, None);

        // Everything on it is 7 h old, but a session in a worktree started for it was seen an
        // hour ago: still worked.
        let old = now() - 7 * 3600;
        db.conn.execute("UPDATE event SET ts = ?1 WHERE issue_key = 'NIT-1'", [old]).unwrap();
        db.conn
            .execute("INSERT INTO event (ts, actor, cwd, issue_key, type, summary) VALUES (?1, 'claude:a', 'C:/wt/nit-1', 'NIT-1', 'worktree', 'Started')", [old])
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('claude:s', 'claude', 'C:/wt/nit-1/src', ?1, ?2, 'working')",
                params![old, now() - 3600],
            )
            .unwrap();
        assert!(db.next_task("NIT", "claude:b").unwrap().is_none(), "a session works in its worktree");
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().track.stuck, None);

        // The session went quiet then too: the work was left.
        db.conn.execute("UPDATE session SET last_seen_at = ?1", [old]).unwrap();
        assert_eq!(db.list_issues(Some("NIT"), false).unwrap()[0].track.stuck.as_deref(), Some("No activity for 7 h"));
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().track.stuck.as_deref(), Some("No activity for 7 h"));
        let again = db.next_task("NIT", "claude:b").unwrap().unwrap();
        assert_eq!((again.key.as_str(), again.claimed_by.as_deref()), ("NIT-1", Some("claude:b")));
        assert_eq!(again.track.stuck, None, "claimed now");
        assert!(db.next_task("NIT", "claude:c").unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn next_task_prefers_active_goals_with_nearer_targets_after_urgency() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        let goal = |db: &Db, title: &str, target: Option<&str>| {
            let change = crate::goals::GoalChange { title: Some(title.into()), outcome: Some("o".into()), target_date: target.map(str::to_string), ..Default::default() };
            db.save_goal("NIT", None, &change, &h).unwrap().id
        };
        let (later, sooner, open, shelved) = (goal(&db, "Later", Some("2027-03-01")), goal(&db, "Sooner", Some("2026-11-01")), goal(&db, "Open", None), goal(&db, "Shelved", Some("2026-10-20")));
        let file = |db: &mut Db, title: &str, priority: i64, goal: Option<i64>| {
            let mut i = new(title);
            i.priority = priority;
            i.goal = goal;
            db.create_issue(&i, &h).unwrap().key
        };
        let plain = file(&mut db, "No goal", 3, None);
        let undated = file(&mut db, "Undated goal", 3, Some(open));
        let late = file(&mut db, "Later goal", 3, Some(later));
        let dropped = file(&mut db, "Dropped goal", 3, Some(shelved));
        let soon = file(&mut db, "Sooner goal", 3, Some(sooner));
        let urgent = file(&mut db, "Urgent, no goal", 2, None);
        db.save_goal("NIT", Some(shelved), &crate::goals::GoalChange { state: Some("dropped".into()), ..Default::default() }, &h).unwrap();
        let order: Vec<String> = std::iter::from_fn(|| db.next_task("NIT", "claude:a").unwrap().map(|i| i.key)).collect();
        assert_eq!(order, [urgent, soon, late, undated, plain, dropped], "priority first, then active goals by target, then the rest by number");
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
    fn file_issues_takes_ten_at_a_time() {
        assert_eq!(FILE_ISSUES_MAX, 10);
        assert!(check_batch_size(10).is_ok());
        let err = check_batch_size(11).unwrap_err().to_string();
        assert!(err.contains("at most 10 per call") && err.contains("file the first 10 now"), "{err}");
        assert!(err.contains("blocked_by") && err.contains("keys"), "says how to name blockers across batches: {err}");

        // The batch itself is not held to it: an approved goal plan files all its issues at once.
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let topics = ["billing", "search", "export", "import", "login", "audit", "themes", "alerts", "uploads", "reports", "webhooks"];
        let items: Vec<BatchItem> = topics
            .iter()
            .map(|t| BatchItem { local_ref: t.to_string(), not_duplicates: vec![], distinct_in_batch: false, issue: NewIssue { allow_similar: true, ..new(&format!("Build the {t} page")) } })
            .collect();
        assert_eq!(db.create_issues(&items, &Actor::Agent("claude:a".into())).unwrap().filed.len(), 11);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn files_a_batch_in_dependency_order_with_local_refs() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:a".into());
        let existing = db.create_issue(&new("Wire the settings page"), &agent).unwrap();
        let item = |r: &str, title: &str, blocked_by: &[&str]| BatchItem {
            not_duplicates: vec![], distinct_in_batch: false,
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

    /// An ask puts its In Progress issue in Needs You, and the answer puts it back: to the agent
    /// that asked while its session is live, else to Todo for the next agent. The asker is told
    /// the answer once (NIT-277).
    #[test]
    fn an_answer_returns_the_issue_and_reaches_the_agent_that_asked() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:s".into());
        db.conn
            .execute("INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('claude:s', 'claude', 'C:/code/app', ?1, ?1, 'working')", [now()])
            .unwrap();
        for title in ["Pick a currency", "Pick a font", "Billing copy"] {
            db.create_issue(&new(title), &agent).unwrap();
        }
        db.move_issue("NIT-1", "In Progress", None, None, &agent).unwrap();
        db.move_issue("NIT-2", "In Progress", None, None, &agent).unwrap();

        let a = db.create_ask("EUR or USD?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&a, &agent).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "Needs You");
        assert!(db.undelivered_answers(&["claude:s"]).unwrap().is_empty(), "nothing to tell yet");
        db.answer_ask(a.id, "EUR", &Actor::Human).unwrap();
        let i = db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((i.state.as_str(), i.claimed_by.as_deref()), ("In Progress", Some("claude:s")), "back to the live asker");
        let told = db.undelivered_answers(&["claude:s", "claude:c:/code/app"]).unwrap();
        assert_eq!(told.iter().map(Ask::outcome).collect::<Vec<_>>(), [format!("NIT-1: the person answered ask {} (\"EUR or USD?\"): EUR", a.id)]);
        db.mark_answers_delivered(&[a.id], "claude:test").unwrap();
        assert!(db.undelivered_answers(&["claude:s"]).unwrap().is_empty(), "told once");

        // Two questions: the first answer leaves it waiting on the second.
        let one = db.create_ask("Which symbol?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&one, &agent).unwrap();
        let two = db.create_ask("Which rounding?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&two, &agent).unwrap();
        db.answer_ask(one.id, "€", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "Needs You", "still waiting on the second");
        db.answer_ask(two.id, "Half up", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "In Progress");

        // A triage question that went to the person, answered last, ends the wait too.
        let three = db.create_ask("Which locale?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&three, &agent).unwrap();
        let escalated = db.create_ask("Is this still wanted?", &[], Some("NIT-1"), None, &Actor::Agent("nitely:triage".into())).unwrap();
        db.answer_ask(three.id, "en-GB", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "Needs You");
        db.answer_ask(escalated.id, "Yes", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "In Progress");

        // An asker with no session record (the CLI's): the work waits in Todo for the next agent.
        let cli = Actor::Agent("cli".into());
        let four = db.create_ask("Which date format?", &[], Some("NIT-1"), None, &cli).unwrap();
        db.wait_on_ask(&four, &cli).unwrap();
        db.answer_ask(four.id, "ISO", &Actor::Human).unwrap();
        let i = db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((i.state.as_str(), i.claimed_by), ("Todo", None));
        db.move_issue("NIT-1", "In Progress", None, None, &agent).unwrap();

        // An asker that is gone: the issue waits in Todo, and next_task gives it to the next agent.
        let b = db.create_ask("Serif?", &[], Some("NIT-2"), None, &agent).unwrap();
        db.wait_on_ask(&b, &agent).unwrap();
        db.conn.execute("UPDATE session SET state = 'ended'", []).unwrap();
        db.answer_ask(b.id, "No", &Actor::Human).unwrap();
        let i = db.issue("NIT-2").unwrap().unwrap();
        assert_eq!((i.state.as_str(), i.claimed_by), ("Todo", None));
        assert_eq!(db.next_task("NIT", "codex:b").unwrap().unwrap().key, "NIT-2");

        // An issue in Needs You for another reason (an approval, say) stays there.
        db.move_issue("NIT-3", "Needs You", None, None, &agent).unwrap();
        let c = db.create_ask("Which wording?", &[], Some("NIT-3"), None, &agent).unwrap();
        db.wait_on_ask(&c, &agent).unwrap();
        db.answer_ask(c.id, "The short one", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-3").unwrap().unwrap().state, "Needs You");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Which wait an answer ends is the one its ask began, recorded on the ask, not guessed from
    /// times (NIT-364): a repeated Needs You minutes later does not strand the answered work, and
    /// an approval wait that begins right after an answer is not ended by a later question's.
    #[test]
    fn an_answer_ends_only_the_wait_its_ask_began() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:s".into());
        db.conn
            .execute("INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('claude:s', 'claude', 'C:/code/app', ?1, ?1, 'working')", [now()])
            .unwrap();
        db.create_issue(&new("Pick a currency"), &agent).unwrap();
        db.move_issue("NIT-1", "In Progress", None, None, &agent).unwrap();

        // Asked, then the issue is moved to Needs You again ten minutes later: still the ask's wait.
        let a = db.create_ask("EUR or USD?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&a, &agent).unwrap();
        db.conn.execute("UPDATE ask SET asked_at = asked_at - 600 WHERE id = ?1", [a.id]).unwrap();
        db.conn.execute("UPDATE event SET ts = ts - 600 WHERE issue_key = 'NIT-1'", []).unwrap();
        db.conn
            .execute("INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'claude:s', 'NIT-1', 'issue_moved', 'Needs You → Needs You')", [now()])
            .unwrap();
        db.answer_ask(a.id, "EUR", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "In Progress", "the repeated move is no new wait");

        // Answered, then a wait for an approval begins at once: a question asked during it does
        // not end it.
        let b = db.create_ask("Which symbol?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&b, &agent).unwrap();
        db.answer_ask(b.id, "€", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "In Progress");
        db.move_issue("NIT-1", "Needs You", None, Some("waits for the person's approval"), &agent).unwrap();
        let c = db.create_ask("Which rounding?", &[], Some("NIT-1"), None, &agent).unwrap();
        db.wait_on_ask(&c, &agent).unwrap();
        db.answer_ask(c.id, "Half up", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-1").unwrap().unwrap().state, "Needs You", "the approval still waits");

        // A submit's approval arriving while an ask waits (Needs You to Needs You, after a review
        // that needs the person): the answer leaves the approval waiting.
        db.create_issue(&new("Billing copy"), &agent).unwrap();
        db.move_issue("NIT-2", "In Progress", None, None, &agent).unwrap();
        let d = db.create_ask("Which tone?", &[], Some("NIT-2"), None, &agent).unwrap();
        db.wait_on_ask(&d, &agent).unwrap();
        db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, ts) VALUES ('NIT-2', 'p', 'c', 'codex', 'pass', 'human', ?1)",
                [now()],
            )
            .unwrap();
        db.answer_ask(d.id, "Plain", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-2").unwrap().unwrap().state, "Needs You", "the approval still waits");

        // A review that asked for changes during the wait is the agent's to act on: the answer
        // still puts the work back.
        db.create_issue(&new("Footer copy"), &agent).unwrap();
        db.move_issue("NIT-3", "In Progress", None, None, &agent).unwrap();
        let e = db.create_ask("Which year?", &[], Some("NIT-3"), None, &agent).unwrap();
        db.wait_on_ask(&e, &agent).unwrap();
        db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, ts) VALUES ('NIT-3', 'p', 'c', 'codex', 'changes', 'human', ?1)",
                [now()],
            )
            .unwrap();
        db.answer_ask(e.id, "This one", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-3").unwrap().unwrap().state, "In Progress");

        // The person accepts the last finding of a review that needs their approval while an ask
        // waits: the change waits for approval, and the answer leaves it waiting.
        let f = db.create_ask("Which month?", &[], Some("NIT-3"), None, &agent).unwrap();
        db.wait_on_ask(&f, &agent).unwrap();
        db.add_note("NIT-3", crate::accept::WAITS_FOR_APPROVAL, crate::accept::AWAITS_APPROVAL, &Actor::Human).unwrap();
        db.answer_ask(f.id, "March", &Actor::Human).unwrap();
        assert_eq!(db.issue("NIT-3").unwrap().unwrap().state, "Needs You", "the approval still waits");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_issue_closing_closes_its_open_asks() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("codex:x".into());
        let done = db.create_issue(&new("Set the origin remote"), &Actor::Human).unwrap();
        let answered = db.create_ask("Which remote?", &[], Some(&done.key), None, &agent).unwrap();
        db.answer_ask(answered.id, "origin", &Actor::Human).unwrap();
        let open = db.create_ask("Push to the fork too?", &[], Some(&done.key), None, &agent).unwrap();
        let other = db.create_issue(&new("Something else"), &Actor::Human).unwrap();
        let elsewhere = db.create_ask("Keep the old flag?", &[], Some(&other.key), None, &agent).unwrap();
        assert!(db.issue(&done.key).unwrap().unwrap().track.asking);

        // Done asked for directly with a question still open is refused (NIT-220): it is answered
        // first, or the issue canceled. Nitely's own closing moves (a land) close it with the issue.
        let refused = db.check_open_asks(&done.key, "Done").unwrap_err().to_string();
        assert!(refused.contains("1 open question: answer it first"), "{refused}");
        assert!(db.check_open_asks(&done.key, " done ").is_err(), "as move_issue reads it");
        assert!(db.check_open_asks(&done.key, "Canceled").is_ok());
        db.move_issue(&done.key, "Done", Some("landed"), None, &agent).unwrap();
        let closed = db.ask(open.id).unwrap().unwrap();
        assert_eq!((closed.answer, closed.closed.as_deref()), (None, Some("closed with NIT-1 (Done)")));
        assert_eq!(db.ask(answered.id).unwrap().unwrap().closed, None, "an answered ask keeps its answer");
        assert_eq!(db.open_asks().unwrap().iter().map(|a| a.id).collect::<Vec<_>>(), [elsewhere.id], "other issues' asks stay");
        assert!(db.answer_ask(open.id, "yes", &Actor::Human).unwrap_err().to_string().contains("needs no answer"));
        let issue = db.issue(&done.key).unwrap().unwrap();
        assert!(!issue.track.asking);
        let last = db.timeline(&done.key).unwrap().into_iter().find(|t| t.kind == "ask_closed").unwrap();
        assert_eq!(last.summary.as_deref(), Some("Closed with the issue, unanswered: Push to the fork too?"));

        // Canceled closes them too; and an ask left open on an issue closed before this rule is
        // not listed either.
        db.move_issue(&other.key, "Canceled", None, Some("not needed"), &Actor::Human).unwrap();
        assert!(db.ask(elsewhere.id).unwrap().unwrap().closed.is_some());
        let stale = db.create_issue(&new("Old work"), &Actor::Human).unwrap();
        let old = db.create_ask("Still there?", &[], Some(&stale.key), None, &agent).unwrap();
        db.conn.execute("UPDATE issue SET state = 'Done', state_type = 'completed' WHERE key = ?1", [&stale.key]).unwrap();
        assert!(db.open_asks().unwrap().is_empty());
        assert_eq!(db.ask(old.id).unwrap().unwrap().closed.as_deref(), Some("closed with NIT-3 (Done)"), "an agent waiting on it hears it closed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn native_issues_carry_their_app_link() {
        assert_eq!(issue_link("nit-14"), "nitely://issue/NIT-14");
        assert_eq!(key_from_link("http://127.0.0.1:7423/i/nit-14"), Some("NIT-14".into()));
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
        // The web form, which chats and GitHub open; the app answers it on loopback.
        assert_eq!(issue.url, crate::links::issue_url(&issue.key));
        assert!(issue.url.starts_with("http://127.0.0.1:") && issue.url.ends_with("/i/NIT-1"), "{}", issue.url);
        assert_eq!(db.list_issues(Some("NIT"), false).unwrap()[0].url, issue.url);

        // An imported issue keeps where it came from, but its link is still the app's.
        let linear = "https://linear.app/x/issue/NIT-1/links";
        db.conn.execute("UPDATE issue SET url = ?2 WHERE key = ?1", rusqlite::params![issue.key, linear]).unwrap();
        let imported = db.issue(&issue.key).unwrap().unwrap();
        assert_eq!((imported.url.as_str(), imported.external_url.as_str()), (issue.url.as_str(), linear));
        assert_eq!(db.list_issues(Some("NIT"), false).unwrap()[0].url, issue.url);
    }

    #[test]
    fn helpers() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_792_125_417), "2026-10-16T04:36:57Z");
        assert_eq!(branch_name("claude", "NIT-12", "Fix the sidebar's width!"), "claude/nit-12-fix-the-sidebar-s-width");
        assert_eq!(parse_state("needs-you").unwrap().0, "Needs You");
    }
}
