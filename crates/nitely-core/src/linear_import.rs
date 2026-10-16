//! The one-time move of a Linear project into native issues, so Linear can be retired (NIT-9).
//! Keys stay as they are (AND-275 stays AND-275), with their state, priority, labels, body and
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
    pub identifier: String,
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

const QUERY: &str = "query($project: ID!, $after: String) {
  issues(first: 50, after: $after, includeArchived: false, filter: { project: { id: { eq: $project } } }) {
    nodes { identifier title description url priority createdAt updatedAt
            state { name type } labels { nodes { name } }
            comments(first: 100) { nodes { body createdAt user { name } } } }
    pageInfo { hasNextPage endCursor } } }";

/// Every issue of the credentials' Linear project, in any state, with its comments.
pub fn fetch(creds: &Credentials) -> Result<Vec<LinearIssue>> {
    if !creds.linear.usable() {
        bail!("the credentials file has no usable Linear section (api_key and project_id)");
    }
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let body = json!({ "query": QUERY, "variables": { "project": creds.linear.project_id, "after": after } });
        let mut res = crate::remote::agent()
            .post("https://api.linear.app/graphql")
            .header("Authorization", &creds.linear.api_key)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .context("calling Linear")?;
        let status = res.status();
        let v: Value = res.body_mut().read_json().context("reading Linear's answer")?;
        if !status.is_success() || v.get("errors").is_some() {
            let msg = v.pointer("/errors/0/message").and_then(Value::as_str).unwrap_or("request failed");
            bail!("Linear {}: {msg}", status.as_u16());
        }
        let (issues, next) = parse_page(&v)?;
        out.extend(issues);
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(out)
}

pub fn parse_page(v: &Value) -> Result<(Vec<LinearIssue>, Option<String>)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Node {
        identifier: String,
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
        labels: Nodes<Label>,
        comments: Nodes<Comment>,
    }
    #[derive(Deserialize)]
    struct State {
        name: String,
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct Nodes<T> {
        nodes: Vec<T>,
    }
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }
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
    let nodes: Vec<Node> = serde_json::from_value(v.pointer("/data/issues/nodes").cloned().unwrap_or(json!([]))).context("Linear's issues have an unexpected shape")?;
    let issues = nodes
        .into_iter()
        .map(|n| {
            let mut comments: Vec<LinearComment> = n
                .comments
                .nodes
                .into_iter()
                .map(|c| LinearComment { author: c.user.map_or_else(|| "someone".into(), |u| u.name), body: c.body, created_at: c.created_at })
                .collect();
            comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            LinearIssue {
                identifier: n.identifier,
                title: n.title,
                description: n.description.unwrap_or_default(),
                url: n.url,
                state_name: n.state.name,
                state_type: n.state.kind,
                priority: n.priority as i64,
                labels: n.labels.nodes.into_iter().map(|l| l.name).collect(),
                created_at: n.created_at,
                updated_at: n.updated_at,
                comments,
            }
        })
        .collect();
    let next = if v.pointer("/data/issues/pageInfo/hasNextPage").and_then(Value::as_bool) == Some(true) {
        v.pointer("/data/issues/pageInfo/endCursor").and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };
    Ok((issues, next))
}

// ----- mapping -----

/// The native state for a Linear state: the same name when Nitely has it ("Needs Andrew" is
/// Needs You), else by its type. Approved becomes Needs You: in Nitely only a human approves,
/// in the app, so a Linear approval is kept as a note to confirm rather than carried over.
pub fn native_state(name: &str, kind: &str) -> &'static str {
    match parse_state(name) {
        Ok(("Approved", _)) => "Needs You",
        Ok((state, _)) => state,
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

/// The issue kind from its labels (Bug, Feature, ...), if one names a Nitely kind.
fn kind_of(labels: &[String]) -> Option<&'static str> {
    labels.iter().find_map(|l| {
        let l = l.to_lowercase().replace(' ', "-");
        KINDS.iter().copied().find(|k| *k == l || (*k == "live-check" && l == "livecheck"))
    })
}

/// Who filed it, from Aera's `from:claude` / `from:andrew` labels; "linear" when no label says.
fn source_of(labels: &[String]) -> String {
    labels
        .iter()
        .find_map(|l| l.strip_prefix("from:"))
        .map(|who| match who.to_lowercase().as_str() {
            "andrew" | "human" => "human".to_string(),
            w if crate::issues::SOURCES.contains(&w) => w.to_string(),
            _ => "linear".to_string(),
        })
        .unwrap_or_else(|| "linear".into())
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

/// What importing `issues` into `project` would do. Changes nothing.
pub fn plan(db: &Db, issues: &[LinearIssue], project: &str, project_name: &str) -> Result<ImportPlan> {
    let project = project.trim().to_uppercase();
    let existing = db.project(&project)?;
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
        *states.entry((issue.state_name.clone(), native_state(&issue.state_name, &issue.state_type).to_string())).or_default() += 1;
        for l in &issue.labels {
            *out.labels.entry(l.clone()).or_default() += 1;
        }
        *out.kinds.entry(kind_of(&issue.labels).unwrap_or("none").to_string()).or_default() += 1;
        *out.sources.entry(source_of(&issue.labels)).or_default() += 1;
        out.comments += issue.comments.len();
        if issue.state_name.eq_ignore_ascii_case("approved") {
            out.notes.push(format!("{} was Approved in Linear; it becomes Needs You with a note, to approve again in Nitely", issue.identifier));
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

/// Imports `issues` into `project` as `plan` says, in one transaction: creates the project if
/// needed, the issues with their keys, and their comments as timeline notes, replacing the
/// Linear mirror rows of the same keys.
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
        let native = native_state(&issue.state_name, &issue.state_type);
        let (state, state_type) = parse_state(native)?;
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
                { "identifier": "AND-2", "title": "Tab strip flickers", "description": "Steps: switch profile.", "url": "https://linear.app/x/issue/AND-2",
                  "priority": 1, "createdAt": "2026-10-01T09:00:00.000Z", "updatedAt": "2026-10-02T09:00:00.000Z",
                  "state": { "name": "Needs Andrew", "type": "started" }, "labels": { "nodes": [{ "name": "Bug" }, { "name": "tabs" }] },
                  "comments": { "nodes": [
                      { "body": "Second", "createdAt": "2026-10-02T08:00:00.000Z", "user": { "name": "Andrew" } },
                      { "body": "First", "createdAt": "2026-10-01T10:00:00.000Z", "user": null } ] } },
                { "identifier": "AND-7", "title": "Reader mode", "description": null, "url": "https://linear.app/x/issue/AND-7",
                  "priority": 0, "createdAt": "2026-09-01T00:00:00Z", "updatedAt": "2026-09-02T00:00:00Z",
                  "state": { "name": "Shipped", "type": "completed" }, "labels": { "nodes": [{ "name": "Feature" }] },
                  "comments": { "nodes": [] } },
                { "identifier": "AND-5", "title": "Ready to land", "url": "u", "priority": 2,
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
        let (issues, next) = parse_page(&page()).unwrap();
        assert_eq!(next.as_deref(), Some("c2"));
        assert_eq!(issues[0].comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), ["First", "Second"], "oldest first");
        assert_eq!(issues[0].comments[0].author, "someone");

        // The mirror already holds AND-2 read-only.
        let mirrored = IssueRecord { key: "AND-2".into(), title: "Tab strip flickers".into(), state: "Needs Andrew".into(), state_type: "started".into(), ..Default::default() };
        db.replace_issues("linear", &[mirrored]).unwrap();

        let p = plan(&db, &issues, "and", "Aera").unwrap();
        assert!(!p.project_exists);
        assert_eq!((p.found, p.create, p.replaces_mirrored, p.comments, p.next_number), (4, 3, 1, 2, 8));
        assert_eq!((p.first_key.as_deref(), p.last_key.as_deref()), (Some("AND-2"), Some("AND-7")));
        assert_eq!(p.other_prefix, ["OPS-1"]);
        let state = |linear: &str| p.states.iter().find(|s| s.linear == linear).map(|s| s.native.clone());
        assert_eq!(state("Needs Andrew").as_deref(), Some("Needs You"));
        assert_eq!(state("Shipped").as_deref(), Some("Done"));
        assert_eq!(state("Approved").as_deref(), Some("Needs You"));
        assert_eq!(p.kinds.get("bug"), Some(&1));
        assert_eq!(p.sources.get("linear"), Some(&3));
        assert_eq!(source_of(&["from:andrew".into()]), "human");
        assert_eq!(source_of(&["x".into(), "from:claude".into()]), "claude");
        assert_eq!(p.notes.len(), 1, "the Linear approval is called out");
        assert!(db.project("AND").unwrap().is_none(), "planning changes nothing");

        let done = apply(&mut db, &issues, &p, &Actor::Human).unwrap();
        assert_eq!((done.created, done.comments), (3, 2));
        let a2 = db.issue("AND-2").unwrap().unwrap();
        assert_eq!((a2.origin.as_str(), a2.state.as_str(), a2.kind.as_deref(), a2.priority), ("nitely", "Needs You", Some("bug"), 1));
        assert_eq!(a2.labels, ["Bug", "tabs"]);
        let timeline = db.timeline("AND-2").unwrap();
        assert_eq!(timeline.iter().map(|t| t.kind.as_str()).collect::<Vec<_>>(), ["issue_created", "note", "note", "imported"]);
        assert_eq!(timeline[1].actor, "linear:someone");
        assert_eq!(db.issue("AND-7").unwrap().unwrap().state, "Done");
        assert!(db.issue("OPS-1").unwrap().is_none());

        // New issues continue after the highest imported number.
        let n = db
            .create_issue(&NewIssue { project: "AND".into(), title: "Next".into(), body: "b".into(), kind: "bug".into(), source: "human".into(), evidence: "e".into(), ..Default::default() }, &Actor::Human)
            .unwrap();
        assert_eq!(n.key, "AND-8");
        // Run again: everything is native already, nothing is created twice.
        let again = plan(&db, &issues, "AND", "Aera").unwrap();
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
