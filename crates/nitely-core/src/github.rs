//! Pull requests and CI checks from GitHub's REST API, for every watched repo whose `origin` is on
//! GitHub. Read-only, apart from opening pull requests and the issue mirror (mirror.rs). The token comes from `NITELY_GITHUB_TOKEN`, `GITHUB_TOKEN`, or the GitHub
//! CLI (`gh auth token`), in that order, so a signed-in `gh` is all a user needs.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::config::Config;
use crate::db::{Db, PrRecord};
use crate::git;
use crate::remote::agent;

pub(crate) const API: &str = "https://api.github.com";
/// Recently updated PRs per repo; open ones also get reviews and checks (3 more calls each).
const PRS_PER_REPO: usize = 50;

pub fn sync(config: &Config, db: &mut Db) -> Result<usize> {
    let client = Client::new();
    let me = client
        .get(&format!("{API}/user"))
        .ok()
        .and_then(|u| u.get("login").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();

    let slugs: BTreeSet<String> = config.repos.iter().filter_map(|r| git::github_slug(&r.root)).collect();
    let mut total = 0;
    for slug in slugs {
        let list = client
            .get(&format!("{API}/repos/{slug}/pulls?state=all&sort=updated&direction=desc&per_page={PRS_PER_REPO}"))
            .with_context(|| format!("pull requests for {slug}"))?;
        let mut prs = Vec::new();
        for pr in list.as_array().cloned().unwrap_or_default() {
            let Some(mut record) = parse_pr(&pr, &slug) else { continue };
            if record.state == "OPEN" {
                // Best effort: a PR still shows when its reviews or checks can't be read.
                if let Ok(reviews) = client.get(&format!("{API}/repos/{slug}/pulls/{}/reviews?per_page=100", record.number)) {
                    let (by, mine) = approvals(&reviews, &me);
                    record.approved_by = by;
                    record.approved_by_me = mine;
                }
                if let Ok(detail) = client.get(&format!("{API}/repos/{slug}/pulls/{}", record.number)) {
                    record.comment_count = ["comments", "review_comments"]
                        .iter()
                        .filter_map(|k| detail.get(*k).and_then(Value::as_i64))
                        .sum();
                }
                if let Ok(runs) = client.get(&format!("{API}/repos/{slug}/commits/{}/check-runs?per_page=100", record.head_sha)) {
                    record.checks = checks_state(&runs);
                }
            }
            prs.push(record);
        }
        total += prs.len();
        db.replace_prs("github", &slug, &prs)?;
    }
    Ok(total)
}

/// Opens the branch's pull request into `base`, or updates the open one. Returns its URL.
pub fn open_pr(slug: &str, branch: &str, base: &str, title: &str, body: &str) -> Result<String> {
    let client = Client::new();
    let owner = slug.split('/').next().unwrap_or("");
    let open = client.get(&format!("{API}/repos/{slug}/pulls?state=open&head={owner}:{branch}"))?;
    let pr = match open.as_array().and_then(|a| a.first()).and_then(|p| p.get("number")).and_then(Value::as_i64) {
        Some(n) => client.send("PATCH", &format!("{API}/repos/{slug}/pulls/{n}"), &serde_json::json!({ "title": title, "body": body }))?,
        None => client.send(
            "POST",
            &format!("{API}/repos/{slug}/pulls"),
            &serde_json::json!({ "title": title, "body": body, "head": branch, "base": base }),
        )?,
    };
    pr.get("html_url").and_then(Value::as_str).map(str::to_string).context("GitHub returned no pull request URL")
}

/// Closes the branch's open pull request with `comment`, for a change that reached its base
/// another way (landed_elsewhere.rs). Returns its URL, or None when it has none open.
pub fn close_pr(slug: &str, branch: &str, comment: &str) -> Result<Option<String>> {
    let client = Client::new();
    let owner = slug.split('/').next().unwrap_or("");
    let open = client.get(&format!("{API}/repos/{slug}/pulls?state=open&head={owner}:{branch}"))?;
    let Some(n) = open.as_array().and_then(|a| a.first()).and_then(|p| p.get("number")).and_then(Value::as_i64) else { return Ok(None) };
    client.send("POST", &format!("{API}/repos/{slug}/issues/{n}/comments"), &serde_json::json!({ "body": comment }))?;
    let pr = client.send("PATCH", &format!("{API}/repos/{slug}/pulls/{n}"), &serde_json::json!({ "state": "closed" }))?;
    Ok(pr.get("html_url").and_then(Value::as_str).map(str::to_string))
}

pub(crate) struct Client {
    token: Option<String>,
    agent: ureq::Agent,
}

impl Client {
    pub(crate) fn new() -> Client {
        Client { token: token(), agent: agent() }
    }

    pub(crate) fn get(&self, url: &str) -> Result<Value> {
        let mut req = self.agent.get(url);
        for (k, v) in self.headers() {
            req = req.header(k, &v);
        }
        let res = req.call().with_context(|| format!("calling {}", url.split('?').next().unwrap_or(url)))?;
        self.read(res)
    }

    pub(crate) fn delete(&self, url: &str) -> Result<()> {
        let mut req = self.agent.delete(url);
        for (k, v) in self.headers() {
            req = req.header(k, &v);
        }
        let mut res = req.call().with_context(|| format!("calling DELETE {url}"))?;
        if !res.status().is_success() && res.status().as_u16() != 404 {
            let text = res.body_mut().read_to_string().unwrap_or_default();
            bail!("GitHub {}: {}", res.status().as_u16(), text.chars().take(200).collect::<String>());
        }
        Ok(())
    }

    pub(crate) fn send(&self, method: &str, url: &str, body: &Value) -> Result<Value> {
        let mut req = match method {
            "POST" => self.agent.post(url),
            "PATCH" => self.agent.patch(url),
            _ => self.agent.put(url),
        };
        for (k, v) in self.headers() {
            req = req.header(k, &v);
        }
        let res = req.send_json(body).with_context(|| format!("calling {method} {url}"))?;
        self.read(res)
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", "2022-11-28".to_string()),
            ("User-Agent", "nitely".to_string()),
        ];
        if let Some(t) = &self.token {
            h.push(("Authorization", format!("Bearer {t}")));
        }
        h
    }

    fn read(&self, mut res: ureq::http::Response<ureq::Body>) -> Result<Value> {
        let status = res.status();
        if !status.is_success() {
            let text = res.body_mut().read_to_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "request failed".into());
            let hint = match status.as_u16() {
                401 => "; sign in with `gh auth login` or set GITHUB_TOKEN",
                403 | 404 if self.token.is_none() => "; no GitHub token found (sign in with `gh auth login`)",
                _ => "",
            };
            bail!("GitHub {}: {msg}{hint}", status.as_u16());
        }
        Ok(res.body_mut().read_json()?)
    }
}

/// `NITELY_GITHUB_TOKEN=none` sends no token, for networks that add one themselves.
fn token() -> Option<String> {
    for var in ["NITELY_GITHUB_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            let t = t.trim().to_string();
            if t == "none" {
                return None;
            }
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    let mut cmd = std::process::Command::new("gh");
    cmd.args(["auth", "token"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !t.is_empty()).then_some(t)
}

pub fn parse_pr(pr: &Value, slug: &str) -> Option<PrRecord> {
    let s = |path: &[&str]| -> String {
        path.iter()
            .try_fold(pr, |acc, k| acc.get(*k))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let merged = pr.get("merged_at").is_some_and(|m| !m.is_null());
    // Bitbucket's vocabulary, so both providers read the same in the app.
    let state = match (s(&["state"]).as_str(), merged) {
        ("open", _) => "OPEN",
        (_, true) => "MERGED",
        _ => "DECLINED",
    };
    Some(PrRecord {
        provider: "github".into(),
        repo: slug.into(),
        number: pr.get("number")?.as_i64()?,
        title: s(&["title"]),
        url: s(&["html_url"]),
        state: state.into(),
        branch: s(&["head", "ref"]),
        head_sha: s(&["head", "sha"]),
        updated_at: s(&["updated_at"]),
        comment_count: 0,
        approved_by: vec![],
        approved_by_me: false,
        checks: String::new(),
    })
}

/// Who approved: each reviewer's latest decisive review (approve, request changes, dismissed).
pub fn approvals(reviews: &Value, me: &str) -> (Vec<String>, bool) {
    let mut latest: Vec<(String, String)> = Vec::new();
    for r in reviews.as_array().into_iter().flatten() {
        let user = r.pointer("/user/login").and_then(Value::as_str).unwrap_or("").to_string();
        let state = r.get("state").and_then(Value::as_str).unwrap_or("");
        if user.is_empty() || !matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue;
        }
        latest.retain(|(u, _)| *u != user);
        latest.push((user, state.to_string()));
    }
    let approved: Vec<String> = latest.into_iter().filter(|(_, s)| s == "APPROVED").map(|(u, _)| u).collect();
    let mine = !me.is_empty() && approved.iter().any(|u| u.eq_ignore_ascii_case(me));
    (approved, mine)
}

/// One word for a commit's check runs: failure beats pending beats success.
pub fn checks_state(runs: &Value) -> String {
    let runs = runs.get("check_runs").and_then(Value::as_array).cloned().unwrap_or_default();
    if runs.is_empty() {
        return String::new();
    }
    let mut pending = false;
    for r in &runs {
        if r.get("status").and_then(Value::as_str) != Some("completed") {
            pending = true;
            continue;
        }
        match r.get("conclusion").and_then(Value::as_str).unwrap_or("") {
            "success" | "neutral" | "skipped" => {}
            _ => return "failure".into(),
        }
    }
    if pending { "pending" } else { "success" }.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_pr_states() {
        let base = json!({ "number": 3, "title": "t", "html_url": "u", "head": { "ref": "claude/nit-7-x", "sha": "abc" } });
        let mut open = base.clone();
        open["state"] = json!("open");
        let mut merged = base.clone();
        merged["state"] = json!("closed");
        merged["merged_at"] = json!("2026-10-16T05:41:49Z");
        let mut closed = base.clone();
        closed["state"] = json!("closed");
        closed["merged_at"] = Value::Null;
        assert_eq!(parse_pr(&open, "o/r").unwrap().state, "OPEN");
        assert_eq!(parse_pr(&merged, "o/r").unwrap().state, "MERGED");
        assert_eq!(parse_pr(&closed, "o/r").unwrap().state, "DECLINED");
        assert_eq!(parse_pr(&open, "o/r").unwrap().branch, "claude/nit-7-x");
    }

    #[test]
    fn latest_review_per_person_counts() {
        let reviews = json!([
            { "user": { "login": "Sam" }, "state": "CHANGES_REQUESTED" },
            { "user": { "login": "sam" }, "state": "COMMENTED" },
            { "user": { "login": "Sam" }, "state": "APPROVED" },
            { "user": { "login": "bot" }, "state": "APPROVED" },
            { "user": { "login": "bot" }, "state": "DISMISSED" }
        ]);
        assert_eq!(approvals(&reviews, "sam"), (vec!["Sam".to_string()], true));
    }

    #[test]
    fn summarizes_checks() {
        let run = |status: &str, conclusion: Value| json!({ "status": status, "conclusion": conclusion });
        let all = |runs: Vec<Value>| checks_state(&json!({ "check_runs": runs }));
        assert_eq!(all(vec![]), "");
        assert_eq!(all(vec![run("completed", json!("success")), run("completed", json!("skipped"))]), "success");
        assert_eq!(all(vec![run("completed", json!("success")), run("in_progress", Value::Null)]), "pending");
        assert_eq!(all(vec![run("in_progress", Value::Null), run("completed", json!("failure"))]), "failure");
    }
}
