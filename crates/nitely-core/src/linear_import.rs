//! The one-time move of a Linear project into native issues, so Linear can be retired (NIT-9).
//! Keys stay as they are (APP-275 stays APP-275), with their state, priority, labels, body and
//! comments; the project's next number follows the highest one. `plan` shows everything it would
//! do and changes nothing; `apply` does it in one transaction.

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::credentials::Credentials;
use crate::db::Db;
use crate::issues::{parse_state, Actor, KINDS};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LinearIssue {
    /// Linear's own id, to page through an issue's comments and labels.
    #[serde(skip)]
    pub id: String,
    pub identifier: String,
    /// Archived in Linear (old finished work, usually).
    pub archived: bool,
    pub title: String,
    pub description: String,
    pub url: String,
    pub state_name: String,
    /// backlog, unstarted, started, completed, canceled (or triage).
    pub state_type: String,
    /// Linear's scale, the same as Nitely's: 0 none, 1 urgent to 4 low.
    pub priority: i64,
    pub labels: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub comments: Vec<LinearComment>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LinearComment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// How a Linear state becomes a native one, and how many issues take that way.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StateMapping {
    pub linear: String,
    pub native: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportPlan {
    pub project: String,
    pub project_name: String,
    /// False: the project is created.
    pub project_exists: bool,
    /// Issues found in Linear.
    pub found: usize,
    /// Issues that would be created, and their keys' range.
    pub create: usize,
    pub first_key: Option<String>,
    pub last_key: Option<String>,
    /// Keys already native here, left as they are.
    pub already_native: Vec<String>,
    /// Keys whose prefix is not the project's, left out.
    pub other_prefix: Vec<String>,
    pub states: Vec<StateMapping>,
    /// Labels and how many issues carry each; every label is kept on its issues.
    pub labels: BTreeMap<String, usize>,
    /// The issue kind taken from a label (bug, feature, ...), or "none".
    pub kinds: BTreeMap<String, usize>,
    /// Who filed them, from `from:` labels, or "linear".
    pub sources: BTreeMap<String, usize>,
    /// How many of the issues to create are archived in Linear.
    pub archived: usize,
    pub comments: usize,
    /// The project's next issue number after the import.
    pub next_number: i64,
    /// Read-only Linear mirror rows replaced by the native issues.
    pub replaces_mirrored: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    pub project: String,
    pub created: usize,
    pub comments: usize,
    pub next_number: i64,
}

// ----- fetching -----

// 25 issues with 50 comments and 50 labels each keeps a page well under Linear's complexity
// limit; an issue with more is paged on its own (MORE_COMMENTS, MORE_LABELS).
const QUERY: &str = "query($project: ID!, $after: String, $archived: Boolean) {
  issues(first: 25, after: $after, includeArchived: $archived, filter: { project: { id: { eq: $project } } }) {
    nodes { id identifier title description url priority createdAt updatedAt archivedAt
            state { name type }
            labels(first: 50) { nodes { name } pageInfo { hasNextPage endCursor } }
            comments(first: 50) { nodes { body createdAt user { name } } pageInfo { hasNextPage endCursor } } }
    pageInfo { hasNextPage endCursor } } }";
const MORE_COMMENTS: &str = "query($id: String!, $after: String) { issue(id: $id) {
  comments(first: 50, after: $after) { nodes { body createdAt user { name } } pageInfo { hasNextPage endCursor } } } }";
const MORE_LABELS: &str = "query($id: String!, $after: String) { issue(id: $id) {
  labels(first: 50, after: $after) { nodes { name } pageInfo { hasNextPage endCursor } } } }";

fn graphql(creds: &Credentials, query: &str, variables: Value) -> Result<Value> {
    let mut res = crate::remote::agent()
        .post("https://api.linear.app/graphql")
        .header("Authorization", &creds.linear.api_key)
        .header("Content-Type", "application/json")
        .send_json(json!({ "query": query, "variables": variables }))
        .context("calling Linear")?;
    let status = res.status();
    let v: Value = res.body_mut().read_json().context("reading Linear's answer")?;
    if !status.is_success() || v.get("errors").is_some() {
        let msg = v.pointer("/errors/0/message").and_then(Value::as_str).unwrap_or("request failed");
        bail!("Linear {}: {msg}", status.as_u16());
    }
    Ok(v)
}

/// Every issue of the Linear project `project_id` (the credentials' own when None), in any
/// state, with all its comments and labels; archived ones too unless `skip_archived`.
pub fn fetch(creds: &Credentials, project_id: Option<&str>, skip_archived: bool) -> Result<Vec<LinearIssue>> {
    let mut creds = creds.clone();
    if let Some(id) = project_id {
        creds.linear.project_id = id.to_string();
    }
    let creds = &creds;
    if !creds.linear.usable() {
        bail!("the credentials file needs a Linear api_key, and a project_id unless you name the Linear project");
    }
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let v = graphql(creds, QUERY, json!({ "project": creds.linear.project_id, "after": after, "archived": !skip_archived }))?;
        let (issues, next) = parse_page(&v)?;
        for (mut issue, more) in issues {
            let mut cursor = more.comments;
            while let Some(c) = cursor {
                let v = graphql(creds, MORE_COMMENTS, json!({ "id": issue.id, "after": c }))?;
                let (comments, next) = parse_comments(v.pointer("/data/issue/comments").unwrap_or(&Value::Null))?;
                issue.comments.extend(comments);
                cursor = next;
            }
            let mut cursor = more.labels;
            while let Some(c) = cursor {
                let v = graphql(creds, MORE_LABELS, json!({ "id": issue.id, "after": c }))?;
                let (labels, next) = parse_labels(v.pointer("/data/issue/labels").unwrap_or(&Value::Null))?;
                issue.labels.extend(labels);
                cursor = next;
            }
            issue.comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            out.push(issue);
        }
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(out)
}

/// Where an issue's comments and labels continue, when a page did not hold them all.
#[derive(Debug, Default, PartialEq)]
pub struct More {
    pub comments: Option<String>,
    pub labels: Option<String>,
}

fn next_cursor(list: &Value) -> Option<String> {
    (list.pointer("/pageInfo/hasNextPage").and_then(Value::as_bool) == Some(true))
        .then(|| list.pointer("/pageInfo/endCursor").and_then(Value::as_str).map(str::to_string))
        .flatten()
}

fn parse_comments(list: &Value) -> Result<(Vec<LinearComment>, Option<String>)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Comment {
        body: String,
        created_at: String,
        user: Option<User>,
    }
    #[derive(Deserialize)]
    struct User {
        name: String,
    }
    let nodes: Vec<Comment> = serde_json::from_value(list.get("nodes").cloned().unwrap_or(json!([]))).context("Linear's comments have an unexpected shape")?;
    let comments = nodes
        .into_iter()
        .map(|c| LinearComment { author: c.user.map_or_else(|| "someone".into(), |u| u.name), body: c.body, created_at: c.created_at })
        .collect();
    Ok((comments, next_cursor(list)))
}

fn parse_labels(list: &Value) -> Result<(Vec<String>, Option<String>)> {
    let names = list
        .get("nodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|l| l.get("name").and_then(Value::as_str)).map(str::to_string).collect())
        .unwrap_or_default();
    Ok((names, next_cursor(list)))
}

/// A page of issues, each with where its comments and labels continue, and the next page's cursor.
pub type Page = (Vec<(LinearIssue, More)>, Option<String>);

pub fn parse_page(v: &Value) -> Result<Page> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        #[serde(default)]
        id: String,
        identifier: String,
        #[serde(default)]
        archived_at: Option<String>,
        title: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        url: String,
        #[serde(default)]
        priority: f64,
        created_at: String,
        updated_at: String,
        state: State,
        labels: Value,
        comments: Value,
    }
    #[derive(Deserialize)]
    struct State {
        name: String,
        #[serde(rename = "type")]
        kind: String,
    }
    let nodes: Vec<Node> = serde_json::from_value(v.pointer("/data/issues/nodes").cloned().unwrap_or(json!([]))).context("Linear's issues have an unexpected shape")?;
    let mut issues = Vec::new();
    for n in nodes {
        let (mut comments, more_comments) = parse_comments(&n.comments)?;
        comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let (labels, more_labels) = parse_labels(&n.labels)?;
        issues.push((
            LinearIssue {
                id: n.id,
                identifier: n.identifier,
                archived: n.archived_at.is_some(),
                title: n.title,
                description: n.description.unwrap_or_default(),
                url: n.url,
                state_name: n.state.name,
                state_type: n.state.kind,
                priority: n.priority as i64,
                labels,
                created_at: n.created_at,
                updated_at: n.updated_at,
                comments,
            },
            More { comments: more_comments, labels: more_labels },
        ));
    }
    let next = if v.pointer("/data/issues/pageInfo/hasNextPage").and_then(Value::as_bool) == Some(true) {
        v.pointer("/data/issues/pageInfo/endCursor").and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };
    Ok((issues, next))
}

// ----- mapping -----

/// The native state for a Linear state: the same name when Nitely has it; Needs You for an open
/// state named "Needs ..." ("Needs Review", "Needs Input": the usual way a Linear team says an
/// issue waits on a person); else by its type. Approved becomes Needs You: in Nitely only a human
/// approves, in the app, so a Linear approval is kept as a note to confirm rather than carried over.
pub fn native_state(name: &str, kind: &str) -> &'static str {
    match parse_state(name) {
        Ok(("Approved", _)) => "Needs You",
        Ok((state, _)) => state,
        Err(_) if waits_on_a_person(name, kind) => "Needs You",
        Err(_) => match kind {
            "backlog" | "triage" => "Backlog",
            "unstarted" => "Todo",
            "started" => "In Progress",
            "completed" => "Done",
            "canceled" => "Canceled",
            _ => "Todo",
        },
    }
}

/// An open Linear state named "Needs ...".
fn waits_on_a_person(name: &str, kind: &str) -> bool {
    let name = name.trim().to_lowercase();
    name.starts_with("needs ") && !matches!(kind, "completed" | "canceled")
}

/// The issue kind from its labels (Bug, Feature, ...), if one names a Nitely kind.
fn kind_of(labels: &[String]) -> Option<&'static str> {
    labels.iter().find_map(|l| {
        let l = l.to_lowercase().replace(' ', "-");
        KINDS.iter().copied().find(|k| *k == l || (*k == "live-check" && l == "livecheck"))
    })
}

/// Who filed it, from a `from:` label: an agent's name (`from:claude`) is kept, any other name
/// is a person (`from:sam` is human); "linear" when no label says.
fn source_of(labels: &[String]) -> String {
    labels
        .iter()
        .find_map(|l| l.strip_prefix("from:"))
        .map(|who| match who.trim().to_lowercase().as_str() {
            "" => "linear".to_string(),
            w if crate::issues::SOURCES.contains(&w) => w.to_string(),
            _ => "human".to_string(),
        })
        .unwrap_or_else(|| "linear".into())
}

/// The native state for `issue`: the one you chose for its Linear state, else `native_state`.
fn mapped_state(issue: &LinearIssue, overrides: &BTreeMap<String, &'static str>) -> &'static str {
    overrides
        .get(&issue.state_name.to_lowercase())
        .copied()
        .unwrap_or_else(|| native_state(&issue.state_name, &issue.state_type))
}

/// Chosen Nitely states for Linear state names, keyed by the Linear name in lowercase: the
/// config's `linear_states`, then `"Needs Review=Needs You"` pairs (`--state`) over them. Approved
/// is refused: in Nitely only a human approves, in the app.
pub fn overrides(config: &BTreeMap<String, String>, pairs: &[String]) -> Result<BTreeMap<String, &'static str>> {
    let mut out = BTreeMap::new();
    let mut add = |linear: &str, native: &str| -> Result<()> {
        let (native, _) = parse_state(native)?;
        if native == "Approved" {
            bail!("a Linear state cannot become Approved: only a human approves, in Nitely");
        }
        out.insert(linear.trim().to_lowercase(), native);
        Ok(())
    };
    for (linear, native) in config {
        add(linear, native).with_context(|| format!("linear_states in {}", crate::paths::config_path().display()))?;
    }
    for pair in pairs {
        let Some((linear, native)) = pair.split_once('=') else {
            bail!("--state takes LINEAR=NITELY, e.g. \"Needs Review=Needs You\"; got {pair:?}");
        };
        add(linear, native)?;
    }
    Ok(out)
}

/// Shows mirrored Linear issues in the Nitely state chosen for their Linear state, if one is, and
/// an open "Needs ..." state as Needs You; every other state keeps its Linear name.
pub fn map_mirrored(issues: &mut [crate::issues::IssueRecord], overrides: &BTreeMap<String, &'static str>) {
    for issue in issues {
        let native = match overrides.get(&issue.state.to_lowercase()) {
            Some(native) => *native,
            None if parse_state(&issue.state).is_err() && waits_on_a_person(&issue.state, &issue.state_type) => "Needs You",
            None => continue,
        };
        if let Ok((state, state_type)) = parse_state(native) {
            issue.state = state.to_string();
            issue.state_type = state_type.to_string();
        }
    }
}

fn number_of(key: &str, project: &str) -> Option<i64> {
    let (prefix, n) = key.split_once('-')?;
    if !prefix.eq_ignore_ascii_case(project) {
        return None;
    }
    n.parse().ok()
}

/// Unix seconds from RFC 3339 UTC (`2026-10-15T12:34:56.789Z`).
pub fn parse_iso(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.trim_end_matches('Z');
    let time = time.split(['+', '.']).next()?;
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next().and_then(|r| r.ok()).unwrap_or(0));
    // Days from civil (Howard Hinnant), the inverse of issues::iso.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

// ----- plan and apply -----

/// What importing `issues` into `project` would do, with `overrides` (from `overrides`)
/// choosing the native state for some Linear states. Changes nothing.
pub fn plan(db: &Db, issues: &[LinearIssue], project: &str, project_name: &str, overrides: &BTreeMap<String, &'static str>) -> Result<ImportPlan> {
    let project = project.trim().to_uppercase();
    // Imported issues keep their Linear keys, so the key must be a project's own, not an old key
    // whose links now lead to another project's issues.
    let existing = db.project_exact(&project)?;
    if existing.is_none() {
        if let Some(now) = db.project(&project)? {
            bail!("{project} was an earlier key of {}; import into {} or another key", now.key, now.key);
        }
    }
    let mut out = ImportPlan {
        project_exists: existing.is_some(),
        project_name: existing.map(|p| p.name).unwrap_or_else(|| project_name.to_string()),
        project: project.clone(),
        found: issues.len(),
        create: 0,
        first_key: None,
        last_key: None,
        already_native: vec![],
        other_prefix: vec![],
        states: vec![],
        labels: BTreeMap::new(),
        kinds: BTreeMap::new(),
        sources: BTreeMap::new(),
        archived: 0,
        comments: 0,
        next_number: 1,
        replaces_mirrored: 0,
        notes: vec![],
    };
    let mut states: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut numbers: Vec<(i64, String)> = Vec::new();
    for issue in issues {
        let Some(n) = number_of(&issue.identifier, &project) else {
            out.other_prefix.push(issue.identifier.clone());
            continue;
        };
        if let Some(have) = db.issue(&issue.identifier)? {
            if have.origin == "nitely" {
                out.already_native.push(issue.identifier.clone());
                continue;
            }
            out.replaces_mirrored += 1;
        }
        out.create += 1;
        numbers.push((n, issue.identifier.clone()));
        *states.entry((issue.state_name.clone(), mapped_state(issue, overrides).to_string())).or_default() += 1;
        for l in &issue.labels {
            *out.labels.entry(l.clone()).or_default() += 1;
        }
        *out.kinds.entry(kind_of(&issue.labels).unwrap_or("none").to_string()).or_default() += 1;
        *out.sources.entry(source_of(&issue.labels)).or_default() += 1;
        out.comments += issue.comments.len();
        out.archived += usize::from(issue.archived);
        if issue.state_name.eq_ignore_ascii_case("approved") {
            out.notes.push(format!("{} was Approved in Linear; it becomes {} with a note, to approve again in Nitely", issue.identifier, mapped_state(issue, overrides)));
        }
    }
    for (linear, native) in states.keys() {
        if parse_state(linear).is_err() && !overrides.contains_key(&linear.to_lowercase()) {
            let why = if linear.trim().to_lowercase().starts_with("needs ") && native == "Needs You" { "as its name says" } else { "by Linear's state type" };
            out.notes.push(format!(
                "Nitely has no state named {linear:?}; its issues become {native} {why}. To choose another, run again with --state \"{linear}=Todo\" (or any state)"
            ));
        }
    }
    for linear in overrides.keys() {
        if !states.keys().any(|(name, _)| name.to_lowercase() == *linear) {
            out.notes.push(format!("no issue to import is in the Linear state {linear:?} that --state or linear_states names"));
        }
    }
    numbers.sort();
    out.first_key = numbers.first().map(|(_, k)| k.clone());
    out.last_key = numbers.last().map(|(_, k)| k.clone());
    let max_existing: i64 = db.conn.query_row("SELECT COALESCE(MAX(next_number), 1) FROM project WHERE key = ?1", [&project], |r| r.get(0))?;
    out.next_number = numbers.last().map_or(1, |(n, _)| n + 1).max(max_existing);
    out.states = states.into_iter().map(|((linear, native), count)| StateMapping { linear, native, count }).collect();
    out.states.sort_by(|a, b| b.count.cmp(&a.count));
    Ok(out)
}

/// Imports `issues` into `project` as `plan` says (its states included), in one transaction:
/// creates the project if needed, the issues with their keys, and their comments as timeline
/// notes, replacing the Linear mirror rows of the same keys.
pub fn apply(db: &mut Db, issues: &[LinearIssue], plan: &ImportPlan, actor: &Actor) -> Result<Imported> {
    let project = plan.project.clone();
    let now = crate::db::now();
    let tx = db.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // The mirror rows go first: a project cannot take a prefix that issues already use.
    tx.execute("DELETE FROM issue WHERE origin = 'linear' AND key LIKE ?1 || '-%'", [&project])?;
    if !plan.project_exists {
        tx.execute("INSERT INTO project (key, name, created_at) VALUES (?1, ?2, ?3)", params![project, plan.project_name, now])?;
    }
    let mut created = 0;
    let mut comments = 0;
    for issue in issues {
        let Some(number) = number_of(&issue.identifier, &project) else { continue };
        if plan.already_native.contains(&issue.identifier) {
            continue;
        }
        let native = plan
            .states
            .iter()
            .find(|m| m.linear == issue.state_name)
            .map_or_else(|| native_state(&issue.state_name, &issue.state_type).to_string(), |m| m.native.clone());
        let (state, state_type) = parse_state(&native)?;
        let filed = parse_iso(&issue.created_at).unwrap_or(now);
        tx.execute(
            "INSERT INTO issue (key, origin, project, number, title, body, url, state, state_type, kind, source, priority, labels,
               evidence, created_at, updated_at)
             VALUES (?1, 'nitely', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?15, ?10, ?11, ?12, ?13, ?14)",
            params![
                issue.identifier,
                project,
                number,
                issue.title,
                issue.description,
                issue.url,
                state,
                state_type,
                kind_of(&issue.labels),
                issue.priority.clamp(0, 4),
                serde_json::to_string(&issue.labels)?,
                format!("Imported from Linear: {}", issue.url),
                issue.created_at,
                issue.updated_at,
                source_of(&issue.labels)
            ],
        )?;
        let event = |ts: i64, who: &str, kind: &str, summary: &str| {
            tx.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, who, issue.identifier, kind, summary],
            )
        };
        event(filed, "linear", "issue_created", &format!("Filed in Linear as {}", issue.state_name))?;
        for c in &issue.comments {
            event(parse_iso(&c.created_at).unwrap_or(filed), &format!("linear:{}", c.author), "note", c.body.trim())?;
            comments += 1;
        }
        let moved = if issue.state_name.eq_ignore_ascii_case("approved") {
            format!("Imported from Linear as {state}; it was Approved there, so approve it again here if it still stands")
        } else {
            format!("Imported from Linear ({}) as {state}", issue.state_name)
        };
        event(now, actor.id(), "imported", &moved)?;
        created += 1;
    }
    tx.execute("UPDATE project SET next_number = MAX(next_number, ?2) WHERE key = ?1", params![project, plan.next_number])?;
    tx.commit()?;
    Ok(Imported { project, created, comments, next_number: plan.next_number })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::{IssueRecord, NewIssue};

    fn page() -> Value {
        json!({ "data": { "issues": {
            "nodes": [
                { "identifier": "APP-2", "title": "Login button flickers", "description": "Steps: open the login page.", "url": "https://linear.app/acme/issue/APP-2",
                  "priority": 1, "createdAt": "2026-10-01T09:00:00.000Z", "updatedAt": "2026-10-02T09:00:00.000Z",
                  "state": { "name": "Needs Review", "type": "started" }, "labels": { "nodes": [{ "name": "Bug" }, { "name": "login" }] },
                  "comments": { "nodes": [
                      { "body": "Second", "createdAt": "2026-10-02T08:00:00.000Z", "user": { "name": "Sam" } },
                      { "body": "First", "createdAt": "2026-10-01T10:00:00.000Z", "user": null } ],
                      "pageInfo": { "hasNextPage": true, "endCursor": "k2" } } },
                { "identifier": "APP-7", "title": "Reader mode", "description": null, "url": "https://linear.app/acme/issue/APP-7",
                  "priority": 0, "createdAt": "2026-09-01T00:00:00Z", "updatedAt": "2026-09-02T00:00:00Z",
                  "state": { "name": "Shipped", "type": "completed" }, "labels": { "nodes": [{ "name": "Feature" }] },
                  "comments": { "nodes": [] }, "archivedAt": "2026-09-20T00:00:00Z" },
                { "identifier": "APP-5", "title": "Ready to land", "url": "u", "priority": 2,
                  "createdAt": "2026-09-05T00:00:00Z", "updatedAt": "2026-09-06T00:00:00Z",
                  "state": { "name": "Approved", "type": "started" }, "labels": { "nodes": [] }, "comments": { "nodes": [] } },
                { "identifier": "OPS-1", "title": "Another team", "url": "u", "priority": 0,
                  "createdAt": "2026-09-05T00:00:00Z", "updatedAt": "2026-09-06T00:00:00Z",
                  "state": { "name": "Todo", "type": "unstarted" }, "labels": { "nodes": [] }, "comments": { "nodes": [] } } ],
            "pageInfo": { "hasNextPage": true, "endCursor": "c2" } } } })
    }

    #[test]
    fn plans_and_imports_a_linear_project_keeping_its_keys() {
        let dir = std::env::temp_dir().join(format!("nitely-linear-{}-{}", std::process::id(), crate::db::now()));
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        let (paged, next) = parse_page(&page()).unwrap();
        assert_eq!(next.as_deref(), Some("c2"));
        assert_eq!(paged[0].1, More { comments: Some("k2".into()), labels: None }, "more comments to page through");
        assert!(paged[1].0.archived && !paged[0].0.archived);
        let issues: Vec<LinearIssue> = paged.into_iter().map(|(i, _)| i).collect();
        assert_eq!(issues[0].comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), ["First", "Second"], "oldest first");
        assert_eq!(issues[0].comments[0].author, "someone");

        // The mirror already holds APP-2 read-only.
        let mirrored = IssueRecord { key: "APP-2".into(), title: "Login button flickers".into(), state: "Needs Review".into(), state_type: "started".into(), ..Default::default() };
        db.replace_issues("linear", &[mirrored]).unwrap();

        // A state Nitely does not have is mapped by its type, and called out in the notes.
        let by_type = plan(&db, &issues, "app", "Acme", &BTreeMap::new()).unwrap();
        assert_eq!(by_type.states.iter().find(|s| s.linear == "Needs Review").map(|s| s.native.as_str()), Some("Needs You"), "a Needs state waits on you");
        assert!(by_type.notes.iter().any(|n| n.contains("as its name says") && n.contains("--state \"Needs Review=")));
        assert_eq!(native_state("Blocked", "started"), "In Progress");
        assert_eq!(native_state("Needs Review", "completed"), "Done");
        let none = BTreeMap::new();
        assert!(overrides(&none, &["Needs Review=Approved".into()]).is_err(), "an import cannot approve");
        assert!(overrides(&none, &["no equals sign".into()]).is_err());
        let configured = BTreeMap::from([("Needs Review".to_string(), "Todo".to_string()), ("Blocked".to_string(), "Needs You".to_string())]);
        let overrides = overrides(&configured, &["needs review = needs-you".into()]).unwrap();
        assert_eq!((overrides["needs review"], overrides["blocked"]), ("Needs You", "Needs You"), "--state wins over the config");

        // The mirror shows a chosen Linear state as its Nitely state, and an open Needs state as
        // Needs You; others keep their Linear name.
        let row = |state: &str| IssueRecord { key: "APP-9".into(), state: state.into(), state_type: "started".into(), ..Default::default() };
        let mut mirror = vec![row("Needs Input"), row("Blocked")];
        map_mirrored(&mut mirror, &BTreeMap::new());
        assert!(mirror[0].needs_human() && mirror[0].state_type == "started");
        assert_eq!(mirror[1].state, "Blocked");
        let mut chosen = vec![row("Needs Review")];
        map_mirrored(&mut chosen, &BTreeMap::from([("needs review".to_string(), "Todo")]));
        assert_eq!(chosen[0].state, "Todo");
        let p = plan(&db, &issues, "app", "Acme", &overrides).unwrap();
        assert!(!p.project_exists);
        assert_eq!((p.found, p.create, p.replaces_mirrored, p.comments, p.next_number), (4, 3, 1, 2, 8));
        assert_eq!((p.first_key.as_deref(), p.last_key.as_deref()), (Some("APP-2"), Some("APP-7")));
        assert_eq!(p.other_prefix, ["OPS-1"]);
        let state = |linear: &str| p.states.iter().find(|s| s.linear == linear).map(|s| s.native.clone());
        assert_eq!(state("Needs Review").as_deref(), Some("Needs You"));
        assert_eq!(state("Shipped").as_deref(), Some("Done"));
        assert_eq!(state("Approved").as_deref(), Some("Needs You"));
        assert_eq!(p.kinds.get("bug"), Some(&1));
        assert_eq!(p.sources.get("linear"), Some(&3));
        assert_eq!(p.archived, 1);
        assert_eq!(source_of(&["from:sam".into()]), "human");
        assert_eq!(source_of(&["x".into(), "from:claude".into()]), "claude");
        assert_eq!(p.notes.len(), 3, "{:?}", p.notes);
        assert!(p.notes[0].contains("APP-5 was Approved"), "the Linear approval is called out");
        assert!(p.notes[1].contains("\"Shipped\""), "and the state mapped by its type");
        assert!(p.notes[2].contains("\"blocked\""), "and a chosen state no issue is in");
        assert!(db.project("APP").unwrap().is_none(), "planning changes nothing");

        let done = apply(&mut db, &issues, &p, &Actor::Human).unwrap();
        assert_eq!((done.created, done.comments), (3, 2));
        let a2 = db.issue("APP-2").unwrap().unwrap();
        assert_eq!((a2.origin.as_str(), a2.state.as_str(), a2.kind.as_deref(), a2.priority), ("nitely", "Needs You", Some("bug"), 1));
        assert_eq!(a2.labels, ["Bug", "login"]);
        let timeline = db.timeline("APP-2").unwrap();
        assert_eq!(timeline.iter().map(|t| t.kind.as_str()).collect::<Vec<_>>(), ["issue_created", "note", "note", "imported"]);
        assert_eq!(timeline[1].actor, "linear:someone");
        assert_eq!(db.issue("APP-7").unwrap().unwrap().state, "Done");
        assert!(db.issue("OPS-1").unwrap().is_none());

        // New issues continue after the highest imported number.
        let n = db
            .create_issue(&NewIssue { project: "APP".into(), title: "Next".into(), body: "b".into(), kind: "bug".into(), source: "human".into(), evidence: "e".into(), ..Default::default() }, &Actor::Human)
            .unwrap();
        assert_eq!(n.key, "APP-8");
        // Run again: everything is native already, nothing is created twice.
        let again = plan(&db, &issues, "APP", "Acme", &overrides).unwrap();
        assert_eq!((again.create, again.already_native.len()), (0, 3));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_linear_times() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-16T04:36:57.123Z"), Some(1_792_125_417));
        assert_eq!(crate::issues::iso(parse_iso("2024-02-29T23:59:59Z").unwrap()), "2024-02-29T23:59:59Z");
        assert_eq!(parse_iso("not a time"), None);
    }
}
