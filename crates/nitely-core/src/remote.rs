//! Pulls pull requests from Bitbucket and issues from Linear into the local database. Read-only
//! on both services: there is no call here that changes anything on them.

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;

use crate::config::Config;
use crate::credentials::{self, Credentials};
use crate::db::{Db, PrRecord};
use crate::issues::IssueRecord;
use crate::git;

const MAX_PR_PAGES: usize = 4;

/// Verifies TLS against the OS certificate store, so a corporate or antivirus proxy that the
/// system trusts works the same as it does in the browser. Honours HTTPS_PROXY.
pub(crate) fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// What one sync did, for the CLI and the app's footer.
#[derive(Debug, Default, serde::Serialize)]
pub struct SyncReport {
    pub credentials_found: bool,
    pub pull_requests: Option<usize>,
    pub issues: Option<usize>,
    pub errors: Vec<String>,
}

pub fn sync(config: &Config, db: &mut Db) -> SyncReport {
    let mut report = SyncReport::default();

    // GitHub needs no credentials file: it uses a token from the environment or `gh`.
    if config.repos.iter().any(|r| git::github_slug(&r.root).is_some()) {
        let result = crate::github::sync(config, db);
        record(db, "github", &result, &mut report.errors);
        report.pull_requests = result.ok();
    }

    let creds = match credentials::load() {
        Ok(Some(c)) => c,
        Ok(None) => return report,
        Err(e) => {
            report.errors.push(format!("{e:#}"));
            return report;
        }
    };
    report.credentials_found = true;

    if creds.bitbucket.usable() {
        let result = sync_bitbucket(config, &creds, db);
        record(db, "bitbucket", &result, &mut report.errors);
        if let Ok(n) = result {
            report.pull_requests = Some(report.pull_requests.unwrap_or(0) + n);
        }
    }
    if creds.linear.usable() {
        let result = fetch_linear(&creds).and_then(|issues| {
            let n = issues.len();
            db.replace_issues("linear", &issues)?;
            Ok(n)
        });
        record(db, "linear", &result, &mut report.errors);
        report.issues = result.ok();
    }
    report
}

pub(crate) fn record(db: &Db, source: &str, result: &Result<usize>, errors: &mut Vec<String>) {
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    if let Some(e) = &error {
        errors.push(format!("{source}: {e}"));
    }
    let _ = db.set_sync_status(source, error.as_deref());
}

/// The Bitbucket repos to read: each configured repo's `origin`, plus the one named in the
/// credentials file (the Aera repo) so it works before any repo is added.
fn bitbucket_repos(config: &Config, creds: &Credentials) -> BTreeSet<String> {
    let mut repos: BTreeSet<String> = config
        .repos
        .iter()
        .filter_map(|r| git::bitbucket_slug(&r.root))
        .collect();
    if !creds.bitbucket.workspace.is_empty() && !creds.bitbucket.repo.is_empty() {
        repos.insert(format!("{}/{}", creds.bitbucket.workspace, creds.bitbucket.repo));
    }
    repos
}

fn sync_bitbucket(config: &Config, creds: &Credentials, db: &mut Db) -> Result<usize> {
    let bb = &creds.bitbucket;
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", bb.email, bb.api_token))
    );
    let me: Value = get_json("https://api.bitbucket.org/2.0/user", &auth)?;
    let my_id = me.get("account_id").and_then(Value::as_str).unwrap_or("").to_string();

    let mut total = 0;
    for slug in bitbucket_repos(config, creds) {
        let mut prs = Vec::new();
        let mut url = Some(format!(
            "https://api.bitbucket.org/2.0/repositories/{slug}/pullrequests\
             ?state=OPEN&state=MERGED&state=DECLINED&pagelen=50&sort=-updated_on&fields=%2Bvalues.participants"
        ));
        for _ in 0..MAX_PR_PAGES {
            let Some(u) = url.take() else { break };
            let page: Value = get_json(&u, &auth).with_context(|| format!("pull requests for {slug}"))?;
            prs.extend(parse_bitbucket_page(&page, &slug, &my_id));
            url = page.get("next").and_then(Value::as_str).map(str::to_string);
        }
        total += prs.len();
        db.replace_prs("bitbucket", &slug, &prs)?;
    }
    Ok(total)
}

pub fn parse_bitbucket_page(page: &Value, slug: &str, my_account_id: &str) -> Vec<PrRecord> {
    let s = |v: &Value, path: &[&str]| -> String {
        path.iter()
            .try_fold(v, |acc, k| acc.get(*k))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let values = page.get("values").and_then(Value::as_array).cloned().unwrap_or_default();
    values
        .iter()
        .filter_map(|pr| {
            let number = pr.get("id")?.as_i64()?;
            let participants = pr.get("participants").and_then(Value::as_array).cloned().unwrap_or_default();
            let approved: Vec<&Value> = participants
                .iter()
                .filter(|p| p.get("approved").and_then(Value::as_bool).unwrap_or(false))
                .collect();
            Some(PrRecord {
                provider: "bitbucket".into(),
                repo: slug.into(),
                number,
                title: s(pr, &["title"]),
                url: s(pr, &["links", "html", "href"]),
                state: s(pr, &["state"]),
                branch: s(pr, &["source", "branch", "name"]),
                head_sha: s(pr, &["source", "commit", "hash"]),
                updated_at: s(pr, &["updated_on"]),
                comment_count: pr.get("comment_count").and_then(Value::as_i64).unwrap_or(0),
                approved_by: approved.iter().map(|p| s(p, &["user", "display_name"])).collect(),
                approved_by_me: !my_account_id.is_empty()
                    && approved.iter().any(|p| s(p, &["user", "account_id"]) == my_account_id),
                checks: String::new(),
            })
        })
        .collect()
}

/// Open issues of the Aera project (completed, canceled and duplicate are left out, as
/// `linear.mjs projectIssues` does).
fn fetch_linear(creds: &Credentials) -> Result<Vec<IssueRecord>> {
    const QUERY: &str = "query($project: ID!, $after: String) {
      issues(first: 100, after: $after, filter: {
        project: { id: { eq: $project } },
        state: { type: { nin: [\"completed\", \"canceled\", \"duplicate\"] } } }) {
        nodes { identifier title url updatedAt state { name type } labels { nodes { name } } }
        pageInfo { hasNextPage endCursor } } }";
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let body = json!({ "query": QUERY, "variables": { "project": creds.linear.project_id, "after": after } });
        let mut res = agent()
            .post("https://api.linear.app/graphql")
            .header("Authorization", &creds.linear.api_key)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .context("calling Linear")?;
        let status = res.status();
        let v: Value = res.body_mut().read_json().context("reading Linear's answer")?;
        if !status.is_success() || v.get("errors").is_some() {
            let msg = v
                .pointer("/errors/0/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            bail!("Linear {}: {msg}", status.as_u16());
        }
        let (issues, next) = parse_linear_page(&v);
        out.extend(issues);
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok(out)
}

pub fn parse_linear_page(v: &Value) -> (Vec<IssueRecord>, Option<String>) {
    #[derive(Deserialize)]
    struct Node {
        identifier: String,
        title: String,
        url: String,
        #[serde(rename = "updatedAt")]
        updated_at: String,
        state: State,
        labels: Labels,
    }
    #[derive(Deserialize)]
    struct State {
        name: String,
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Deserialize)]
    struct Labels {
        nodes: Vec<Label>,
    }
    #[derive(Deserialize)]
    struct Label {
        name: String,
    }

    let nodes: Vec<Node> = v
        .pointer("/data/issues/nodes")
        .cloned()
        .and_then(|n| serde_json::from_value(n).ok())
        .unwrap_or_default();
    let issues = nodes
        .into_iter()
        .map(|n| IssueRecord {
            origin: "linear".into(),
            key: n.identifier,
            title: n.title,
            url: n.url,
            state: n.state.name,
            state_type: n.state.kind,
            labels: n.labels.nodes.into_iter().map(|l| l.name).collect(),
            updated_at: n.updated_at,
            ..Default::default()
        })
        .collect();
    let next = if v.pointer("/data/issues/pageInfo/hasNextPage").and_then(Value::as_bool) == Some(true) {
        v.pointer("/data/issues/pageInfo/endCursor").and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };
    (issues, next)
}

fn get_json(url: &str, auth: &str) -> Result<Value> {
    let mut res = agent()
        .get(url)
        .header("Authorization", auth)
        .header("Accept", "application/json")
        .call()
        .with_context(|| format!("calling {}", url.split('?').next().unwrap_or(url)))?;
    let status = res.status();
    if !status.is_success() {
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| match status.as_u16() {
                401 | 403 => format!("token rejected; check bitbucket in {}", credentials::path().display()),
                _ => "request failed".into(),
            });
        bail!("Bitbucket {}: {msg}", status.as_u16());
    }
    Ok(res.body_mut().read_json()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bitbucket_prs_and_my_approval() {
        let page = json!({ "values": [{
            "id": 2, "title": "AND-268 tooling", "state": "OPEN", "comment_count": 3,
            "updated_on": "2026-10-15T20:00:00Z",
            "links": { "html": { "href": "https://bitbucket.org/quixet/aera-browser/pull-requests/2" } },
            "source": { "branch": { "name": "claude/and-268-tooling" }, "commit": { "hash": "abc123" } },
            "participants": [
                { "approved": true, "user": { "display_name": "Andrew Rivers", "account_id": "me" } },
                { "approved": false, "user": { "display_name": "Bot", "account_id": "bot" } }
            ]
        }]});
        let prs = parse_bitbucket_page(&page, "quixet/aera-browser", "me");
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].branch, "claude/and-268-tooling");
        assert_eq!(prs[0].approved_by, vec!["Andrew Rivers"]);
        assert!(prs[0].approved_by_me);
    }

    #[test]
    fn parses_linear_page_and_cursor() {
        let v = json!({ "data": { "issues": {
            "nodes": [{ "identifier": "AND-12", "title": "Fix it", "url": "https://linear.app/x/issue/AND-12",
                        "updatedAt": "2026-10-15T00:00:00Z", "state": { "name": "Needs Andrew", "type": "started" },
                        "labels": { "nodes": [{ "name": "bug" }, { "name": "from:claude" }] } }],
            "pageInfo": { "hasNextPage": true, "endCursor": "c1" } } } });
        let (issues, next) = parse_linear_page(&v);
        assert_eq!(issues[0].state, "Needs Andrew");
        assert_eq!(issues[0].labels, vec!["bug", "from:claude"]);
        assert_eq!(next.as_deref(), Some("c1"));
    }
}
