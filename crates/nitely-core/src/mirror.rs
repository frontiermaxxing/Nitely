//! The mirror: a project's issues published to its repo's own tracker, GitHub Issues or
//! Bitbucket's, so people who look at the repo see why each pull request exists (docs/mirror.md).
//! One way: Nitely stays the source of truth, and nothing said on the host is read back. Off by
//! default; only the person turns it on, in the app, after seeing what would be published.
//!
//! Before anything leaves the machine, local paths and agent ids are rewritten to neutral forms
//! and the text goes through the private terms (private_terms.rs). An issue the person marked
//! private, the human's own work, a P1 bug and anything labelled `security` stay private.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

use crate::config::Config;
use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::paths::normalize;
use crate::private_terms::PrivateTerms;

/// Host writes per sync, so a big backfill goes out over a few minutes, inside the hosts' limits.
const WRITES_PER_SYNC: usize = 30;

/// Timeline entries that become comments: what an outsider can follow.
const COMMENTED: &[&str] = &["handoff", "submit", "landed", "issue_moved"];

/// An issue as the host shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostIssue {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    /// None while open; `completed` (Done) or `not_planned` (Canceled, Duplicate) once closed.
    pub closed: Option<&'static str>,
    /// Nitely's kind and priority, for hosts with fields for them (Bitbucket).
    pub kind: String,
    pub priority: i64,
}

impl HostIssue {
    fn fingerprint(&self) -> String {
        use sha2::Digest;
        let text = format!("{}\n{}\n{}\n{:?}", self.title, self.body, self.labels.join(","), self.closed);
        sha2::Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// What the mirror does with an issue.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Publish(HostIssue),
    /// Never published: the person marked it private, or it is sensitive by default.
    Private(String),
    /// Would be published, but its text holds a private term.
    Held(String),
}

// ----- what goes out -----

/// Text with local paths and agent ids made neutral: a path inside one of `roots` (a repo's main
/// checkout, or a worktree beside it) becomes repo-relative, any other absolute path `<local path>`,
/// and an agent id like `claude:c:/code/app-worktrees/app-3` just `claude`.
/// Marks an entry of `roots` as a folder of worktrees (`<folder>/<task>/…`), not a main checkout.
pub const WORKTREES: &str = "worktrees:";

pub fn sanitize(text: &str, roots: &[String]) -> String {
    sanitize_with_home(text, roots, dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).as_deref())
}

/// As `sanitize`, with the home folder given: it is taken out whole first, so a user name with a
/// space in it (`C:\Users\Sam Lee`) never goes out in part.
pub fn sanitize_with_home(text: &str, roots: &[String], home: Option<&str>) -> String {
    let agent = regex::Regex::new(r#"\b(claude|codex|cursor|agent|crew|cli)(@[\w.-]+|:[A-Za-z]:[\\/](?:[^\\/\n"'`<>|:]*[^\s\\/"'`<>|:][\\/])*[^\s\\/"'`<>|,;)\]]*|:/(?:[^/\n"'`<>|]*[^\s/"'`<>|]/)*[^\s/"'`<>|,;)\]]*)"#).unwrap();
    let mut text = agent.replace_all(text, "$1").into_owned();
    if let Some(home) = home.map(|h| h.trim_end_matches(['/', '\\'])).filter(|h| h.len() > 3) {
        // Both slash styles, any case: what is inside a repo root keeps its relative part below.
        let pattern = home.split(['/', '\\']).map(regex::escape).collect::<Vec<_>>().join(r"[\\/]");
        if let Ok(re) = regex::Regex::new(&format!("(?i){pattern}")) {
            let keep: Vec<String> = roots.iter().map(|r| normalize(r)).collect();
            text = re
                .replace_all(&text, |c: &regex::Captures| {
                    let found = c.get(0).unwrap();
                    // A repo inside the home folder: leave it for the root rewrite below.
                    let rest = &text[found.end()..];
                    let candidate = normalize(&format!("{}{}", found.as_str(), rest.split_whitespace().next().unwrap_or("")));
                    if keep.iter().any(|r| candidate.starts_with(r.as_str())) { found.as_str().to_string() } else { "<home>".to_string() }
                })
                .into_owned();
        }
    }
    // Windows (C:\x, C:/x) and Unix home and temp paths. Folders may have spaces in their names;
    // the last part, the file, ends at a space.
    // A folder's name may hold spaces but not end with one, so prose between two paths is not taken
    // for a folder.
    let path = regex::Regex::new(r#"(?i)\b[A-Z]:[\\/](?:[^\\/\n"'`<>|:]*[^\s\\/"'`<>|:][\\/])*[^\s\\/"'`<>|,;)\]]*|(?:^|[\s"'`(\[])(/[^\s/"'`<>|,;)\]]+/(?:[^/\n"'`<>|]*[^\s/"'`<>|]/)*[^\s/"'`<>|,;)\]]*)"#).unwrap();
    let roots: Vec<String> = roots.iter().map(|r| match r.strip_prefix(WORKTREES) { Some(d) => format!("{WORKTREES}{}", normalize(d)), None => normalize(r) }).collect();
    let text = regex::Regex::new(r#"<home>(?:[\\/](?:[^\\/\n"'`<>|:]*[^\s\\/"'`<>|:][\\/])*[^\s\\/"'`<>|,;)\]]*)?"#).unwrap()
        .replace_all(&text, |c: &regex::Captures| {
            // A sentence's full stop after the path stays.
            let dots = c[0].len() - c[0].trim_end_matches('.').len();
            format!("<local path>{}", ".".repeat(dots))
        })
        .into_owned();
    path.replace_all(&text, |c: &regex::Captures| {
        let whole = c.get(0).unwrap().as_str();
        let (lead, p) = match c.get(1) {
            Some(m) => (&whole[..m.start() - c.get(0).unwrap().start()], m.as_str()),
            None => ("", whole),
        };
        // A sentence's full stop after the path stays.
        let trimmed = p.trim_end_matches('.');
        let dots = ".".repeat(p.len() - trimmed.len());
        let p = trimmed;
        let n = normalize(p);
        let relative = roots.iter().find_map(|r| match r.strip_prefix(WORKTREES) {
            // A worktree folder: what is inside a task's worktree, after the task's own folder.
            Some(dir) => n.strip_prefix(&format!("{dir}/"))?.split_once('/').map(|(_, inner)| inner.to_string()),
            // The repo's main checkout.
            None => n.strip_prefix(&format!("{r}/")).map(str::to_string),
        });
        // Keep the original case for the relative part.
        let shown = match relative {
            Some(rel) if !rel.is_empty() => p.replace('\\', "/")[p.len() - rel.len()..].to_string(),
            _ => "<local path>".to_string(),
        };
        format!("{lead}{shown}{dots}")
    })
    .into_owned()
}

/// The issue as it goes to the host.
pub fn render(issue: &IssueRecord, roots: &[String]) -> HostIssue {
    let priority = match issue.priority {
        1 => "urgent",
        2 => "high",
        3 => "medium",
        4 => "low",
        _ => "",
    };
    let mut labels: Vec<String> = issue.kind.iter().cloned().collect();
    if !priority.is_empty() {
        labels.push(format!("priority: {priority}"));
    }
    // A label is a word; one that held a path is left out rather than published.
    labels.extend(issue.labels.iter().filter(|l| sanitize(l, roots) == **l).cloned());
    let closed = match issue.state_type.as_str() {
        "completed" => Some("completed"),
        "canceled" => Some("not_planned"),
        _ => None,
    };
    let mut body = sanitize(issue.body.trim(), roots);
    let evidence = sanitize(issue.evidence.trim(), roots);
    if !evidence.is_empty() && evidence != "none yet" {
        body.push_str(&format!("\n\n**Evidence:** {evidence}"));
    }
    let facts: Vec<String> = [Some(issue.state.clone()), issue.kind.clone(), (!priority.is_empty()).then(|| format!("{priority} priority"))].into_iter().flatten().collect();
    body.push_str(&format!("\n\n---\nTracked in Nitely as {} ({}). Nitely is where it is decided; comments here are not read.", issue.key, facts.join(", ")));
    HostIssue { title: sanitize(issue.title.trim(), roots), body, labels, closed, kind: issue.kind.clone().unwrap_or_default(), priority: issue.priority }
}

/// Which private term `text` holds, as "entry n of <list>", never the term itself.
pub fn private_term_in(terms: &PrivateTerms, project: &str, text: &str) -> Result<Option<String>> {
    crate::private_terms::find_in_text(terms, Some(project), text)
}

/// What the mirror does with `issue`: private when the person said so or it is sensitive by
/// default, held when its text holds a private term, else the rendered issue.
pub fn decide(issue: &IssueRecord, marked_private: bool, terms: &PrivateTerms, roots: &[String]) -> Result<Decision> {
    if marked_private {
        return Ok(Decision::Private("marked private".into()));
    }
    if issue.owner == "human" {
        return Ok(Decision::Private("the human's own work stays private".into()));
    }
    if issue.labels.iter().any(|l| l == "security") {
        return Ok(Decision::Private("labelled security".into()));
    }
    if issue.priority == 1 && issue.kind.as_deref() == Some("bug") {
        return Ok(Decision::Private("an urgent bug may be a security problem; mark it public to publish it".into()));
    }
    let out = render(issue, roots);
    let project = issue.project.as_deref().unwrap_or_default();
    let all = format!("{}\n{}\n{}", out.title, out.body, out.labels.join("\n"));
    if let Some(at) = private_term_in(terms, project, &all)? {
        return Ok(Decision::Held(format!("a private term ({at}) is in its text")));
    }
    Ok(Decision::Publish(out))
}

/// The comment a timeline entry becomes, if it is one an outsider can follow. `pr` is the issue's
/// pull request, linked from review outcomes.
pub fn comment_for(kind: &str, summary: &str, pr: Option<&str>, roots: &[String]) -> Option<String> {
    let summary = sanitize(summary.trim(), roots);
    let text = match kind {
        "handoff" => format!("**Handoff note:** {summary}"),
        "submit" => {
            let link = pr.map(|u| format!(" ({u})")).unwrap_or_default();
            // "Review by codex passed; ready to land. Pull request: …" without the repeated link.
            let said = summary.split(" Pull request:").next().unwrap_or(&summary).trim();
            format!("**Review:** {said}{link}")
        }
        "landed" => format!("**Landed:** {summary}"),
        "issue_moved" => {
            let to = summary.split('→').nth(1)?.trim();
            let (state, why) = to.split_once(':').map(|(s, w)| (s.trim(), w.trim())).unwrap_or((to, ""));
            match state {
                "Canceled" | "Duplicate" => format!("**{state}** in Nitely{}", if why.is_empty() { String::new() } else { format!(": {why}") }),
                "Done" => format!("**Done** in Nitely{}", if why.is_empty() { String::new() } else { format!(": {why}") }),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(text)
}

// ----- the hosts -----

/// A repo's issue tracker.
pub trait Host {
    /// `GitHub` or `Bitbucket`.
    fn name(&self) -> &'static str;
    /// `owner/repo` or `workspace/repo`.
    fn repo(&self) -> &str;
    /// Fails, saying why, when the repo has no issue tracker or the token cannot write to it.
    fn check(&self) -> Result<()>;
    /// Creates the issue: its number and address.
    fn create(&self, issue: &HostIssue) -> Result<(i64, String)>;
    fn update(&self, number: i64, issue: &HostIssue) -> Result<()>;
    /// Posts a comment: its id, to take it down later.
    fn comment(&self, number: i64, body: &str) -> Result<i64>;
    fn delete_comment(&self, number: i64, id: i64) -> Result<()>;
    /// Deletes the issue, edit history and all. Hosts keep an edited text's earlier versions, so
    /// this is the only way to take one down for good; it needs admin rights on GitHub.
    fn delete_issue(&self, number: i64) -> Result<()>;
}

struct GitHub {
    slug: String,
}

impl Host for GitHub {
    fn name(&self) -> &'static str {
        "GitHub"
    }
    fn repo(&self) -> &str {
        &self.slug
    }
    fn check(&self) -> Result<()> {
        let repo = crate::github::Client::new().get(&format!("{}/repos/{}", crate::github::API, self.slug))?;
        if repo["has_issues"] == false {
            bail!("issues are turned off on github.com/{}; turn them on in the repo's settings to mirror", self.slug);
        }
        // Admin, so a withdrawn issue can be deleted: an edited text stays readable in its history.
        if repo["permissions"]["admin"] != true {
            bail!("the GitHub token needs admin rights on {}, so an issue made private later can be deleted with its edit history", self.slug);
        }
        Ok(())
    }
    fn create(&self, issue: &HostIssue) -> Result<(i64, String)> {
        let client = crate::github::Client::new();
        let api = format!("{}/repos/{}/issues", crate::github::API, self.slug);
        let made = client.send("POST", &api, &json!({ "title": issue.title, "body": issue.body, "labels": issue.labels }))?;
        let number = made["number"].as_i64().context("GitHub returned no issue number")?;
        Ok((number, made["html_url"].as_str().unwrap_or_default().to_string()))
    }
    fn update(&self, number: i64, issue: &HostIssue) -> Result<()> {
        let mut body = json!({ "title": issue.title, "body": issue.body, "labels": issue.labels, "state": if issue.closed.is_some() { "closed" } else { "open" } });
        if let Some(reason) = issue.closed {
            body["state_reason"] = json!(reason);
        }
        crate::github::Client::new().send("PATCH", &format!("{}/repos/{}/issues/{number}", crate::github::API, self.slug), &body)?;
        Ok(())
    }
    fn comment(&self, number: i64, body: &str) -> Result<i64> {
        let made = crate::github::Client::new().send("POST", &format!("{}/repos/{}/issues/{number}/comments", crate::github::API, self.slug), &json!({ "body": body }))?;
        made["id"].as_i64().context("GitHub returned no comment id")
    }
    fn delete_comment(&self, _number: i64, id: i64) -> Result<()> {
        crate::github::Client::new().delete(&format!("{}/repos/{}/issues/comments/{id}", crate::github::API, self.slug))
    }
    fn delete_issue(&self, number: i64) -> Result<()> {
        let client = crate::github::Client::new();
        let issue = client.get(&format!("{}/repos/{}/issues/{number}", crate::github::API, self.slug))?;
        let id = issue["node_id"].as_str().context("GitHub returned no issue id")?;
        let done = client.send("POST", &format!("{}/graphql", crate::github::API), &json!({
            "query": "mutation($id: ID!) { deleteIssue(input: { issueId: $id }) { clientMutationId } }",
            "variables": { "id": id }
        }))?;
        if let Some(e) = done["errors"].as_array().and_then(|e| e.first()) {
            bail!("GitHub would not delete the issue: {}", e["message"].as_str().unwrap_or("no reason given"));
        }
        Ok(())
    }
}

struct Bitbucket {
    slug: String,
    auth: String,
}

impl Bitbucket {
    fn api(&self) -> String {
        format!("https://api.bitbucket.org/2.0/repositories/{}", self.slug)
    }
    fn fields(issue: &HostIssue) -> Value {
        // Bitbucket has no labels: they go in the text.
        let labels = if issue.labels.is_empty() { String::new() } else { format!("\n\nLabels: {}", issue.labels.join(", ")) };
        let kind = match issue.kind.as_str() {
            "bug" => "bug",
            "feature" | "improvement" => "enhancement",
            "idea" => "proposal",
            _ => "task",
        };
        let priority = match issue.priority {
            1 => "critical",
            2 => "major",
            4 => "trivial",
            _ => "minor",
        };
        let state = match issue.closed {
            Some("completed") => "resolved",
            Some(_) => "wontfix",
            None => "open",
        };
        json!({ "title": issue.title, "content": { "raw": format!("{}{labels}", issue.body) }, "kind": kind, "priority": priority, "state": state })
    }
}

impl Host for Bitbucket {
    fn name(&self) -> &'static str {
        "Bitbucket"
    }
    fn repo(&self) -> &str {
        &self.slug
    }
    fn check(&self) -> Result<()> {
        let repo = crate::remote::bitbucket_get(&self.api(), &self.auth)?;
        if repo["has_issues"] != true {
            bail!("{} has no issue tracker on Bitbucket (many repos have it off); turn it on in the repo's settings to mirror", self.slug);
        }
        Ok(())
    }
    fn create(&self, issue: &HostIssue) -> Result<(i64, String)> {
        let made = crate::remote::bitbucket_send("POST", &format!("{}/issues", self.api()), &self.auth, &Self::fields(issue))?;
        let number = made["id"].as_i64().context("Bitbucket returned no issue id")?;
        Ok((number, made.pointer("/links/html/href").and_then(Value::as_str).unwrap_or_default().to_string()))
    }
    fn update(&self, number: i64, issue: &HostIssue) -> Result<()> {
        crate::remote::bitbucket_send("PUT", &format!("{}/issues/{number}", self.api()), &self.auth, &Self::fields(issue))?;
        Ok(())
    }
    fn comment(&self, number: i64, body: &str) -> Result<i64> {
        let made = crate::remote::bitbucket_send("POST", &format!("{}/issues/{number}/comments", self.api()), &self.auth, &json!({ "content": { "raw": body } }))?;
        made["id"].as_i64().context("Bitbucket returned no comment id")
    }
    fn delete_comment(&self, number: i64, id: i64) -> Result<()> {
        crate::remote::bitbucket_delete(&format!("{}/issues/{number}/comments/{id}", self.api()), &self.auth)
    }
    fn delete_issue(&self, number: i64) -> Result<()> {
        crate::remote::bitbucket_delete(&format!("{}/issues/{number}", self.api()), &self.auth)
    }
}

/// The tracker of the repo linked to `project`, and that repo's root.
pub fn host_for(config: &Config, project: &str) -> Result<(Box<dyn Host>, String)> {
    let repos: Vec<&crate::RepoConfig> = config.repos.iter().filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))).collect();
    for r in &repos {
        if let Some(slug) = crate::git::github_slug(&r.root) {
            return Ok((Box::new(GitHub { slug }), r.root.clone()));
        }
        if let Some(slug) = crate::git::bitbucket_slug(&r.root) {
            let creds = crate::credentials::load()?.context("no Bitbucket credentials file, so Nitely cannot write to Bitbucket")?;
            return Ok((Box::new(Bitbucket { slug, auth: crate::remote::bitbucket_auth(&creds) }), r.root.clone()));
        }
    }
    match repos.is_empty() {
        true => bail!("no repo is linked to {project}"),
        false => bail!("{project}'s repo has no origin on GitHub or Bitbucket, so there is no issue tracker to mirror to"),
    }
}

/// Folders whose paths are rewritten repo-relative: each linked repo's main checkout.
/// Folders whose paths are rewritten repo-relative: each linked repo's main checkout, and the folder
/// its playbook puts worktrees in (marked `WORKTREES`).
fn roots_of(config: &Config, project: &str) -> Vec<String> {
    let probe = IssueRecord { key: "X-1".into(), title: "x".into(), ..Default::default() };
    config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
        .flat_map(|r| {
            let pb = crate::playbook::Playbook::load(Path::new(&r.root)).unwrap_or_default();
            let worktrees = pb.worktree_for(Path::new(&r.root), "x", &probe).parent().map(|d| format!("{WORKTREES}{}", d.to_string_lossy()));
            std::iter::once(r.root.clone()).chain(worktrees)
        })
        .collect()
}

// ----- state -----

/// Which issues a newly turned-on mirror also publishes: only new ones, the open ones too, or all.
pub const BACKFILLS: &[&str] = &["none", "open", "all"];

#[derive(Debug, Clone, Serialize)]
pub struct MirrorSetting {
    pub project: String,
    pub enabled_at: i64,
    pub backfill: String,
}

/// An issue's place in the mirror.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IssueMirror {
    pub issue: String,
    /// `mirrored`, `private`, `held` or `withdrawn` (published, then made private).
    pub status: String,
    pub reason: Option<String>,
    pub host: Option<String>,
    pub number: Option<i64>,
    pub url: Option<String>,
    /// The person marked it private.
    pub marked_private: bool,
}

impl Db {
    pub fn mirror_setting(&self, project: &str) -> Result<Option<MirrorSetting>> {
        Ok(self
            .conn
            .query_row("SELECT project, enabled_at, backfill FROM mirror_project WHERE project = ?1 COLLATE NOCASE", [project], |r| {
                Ok(MirrorSetting { project: r.get(0)?, enabled_at: r.get(1)?, backfill: r.get(2)? })
            })
            .optional()?)
    }

    /// Turns the mirror on (with a backfill) or off. Only the person, in the app, does this.
    pub fn set_mirror(&self, project: &str, on: Option<&str>, actor: &Actor) -> Result<()> {
        if *actor != Actor::Human {
            bail!("only the person turns the mirror on or off, in the app: it publishes issues outside this machine");
        }
        let project = self.project(project)?.with_context(|| format!("no project {project}"))?.key;
        match on {
            Some(backfill) => {
                if !BACKFILLS.contains(&backfill) {
                    bail!("backfill is one of {}", BACKFILLS.join(", "));
                }
                self.conn.execute(
                    "INSERT INTO mirror_project (project, enabled_at, backfill, enabled_by) VALUES (?1, ?2, ?3, 'human')
                     ON CONFLICT (project) DO UPDATE SET backfill = excluded.backfill",
                    params![project, now(), backfill],
                )?;
            }
            None => {
                self.conn.execute("DELETE FROM mirror_project WHERE project = ?1", [&project])?;
            }
        }
        Ok(())
    }

    /// Marks an issue private (never mirrored, withdrawn if it was) or lets it be mirrored again.
    /// Only the person does this; agents keep things private with the `security` label.
    pub fn set_issue_private(&self, key: &str, private: bool, actor: &Actor) -> Result<()> {
        if *actor != Actor::Human {
            bail!("only the person marks an issue private or public; label it security to keep it off the host");
        }
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        match private {
            true => self.conn.execute("INSERT OR IGNORE INTO issue_private (issue_key, at) VALUES (?1, ?2)", params![issue.key, now()])?,
            false => self.conn.execute("DELETE FROM issue_private WHERE issue_key = ?1", [&issue.key])?,
        };
        Ok(())
    }

    fn marked_private(&self, key: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS (SELECT 1 FROM issue_private WHERE issue_key = ?1 COLLATE NOCASE)", [key], |r| r.get(0))?)
    }

    /// An issue's place in the mirror; None when its project does not mirror and it never did.
    pub fn issue_mirror(&self, key: &str) -> Result<Option<IssueMirror>> {
        let marked = self.marked_private(key)?;
        let row = self
            .conn
            .query_row("SELECT issue_key, status, reason, host, number, url FROM issue_mirror WHERE issue_key = ?1 COLLATE NOCASE", [key], |r| {
                Ok(IssueMirror { issue: r.get(0)?, status: r.get(1)?, reason: r.get(2)?, host: r.get(3)?, number: r.get(4)?, url: r.get(5)?, marked_private: false })
            })
            .optional()?;
        Ok(row.map(|m| IssueMirror { marked_private: marked, ..m }).or_else(|| marked.then(|| IssueMirror { issue: key.to_uppercase(), status: "private".into(), reason: Some("marked private".into()), marked_private: true, ..Default::default() })))
    }

    fn save_mirror(&self, key: &str, status: &str, reason: Option<&str>, host: Option<(&str, &str, i64, &str)>, fingerprint: Option<&str>) -> Result<()> {
        let (h, repo, number, url) = host.map(|(h, r, n, u)| (Some(h), Some(r), Some(n), Some(u))).unwrap_or((None, None, None, None));
        self.conn.execute(
            "INSERT INTO issue_mirror (issue_key, status, reason, host, repo, number, url, fingerprint, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (issue_key) DO UPDATE SET status = excluded.status, reason = excluded.reason,
               host = COALESCE(excluded.host, host), repo = COALESCE(excluded.repo, repo), number = COALESCE(excluded.number, number),
               url = COALESCE(excluded.url, url), fingerprint = COALESCE(excluded.fingerprint, fingerprint), updated_at = excluded.updated_at",
            params![key, status, reason, h, repo, number, url, fingerprint, now()],
        )?;
        Ok(())
    }
}

// ----- turning it on: what would go out -----

#[derive(Debug, Clone, Default, Serialize)]
pub struct Preview {
    pub project: String,
    pub enabled: Option<MirrorSetting>,
    pub host: Option<String>,
    pub repo: Option<String>,
    /// Why it cannot mirror, when it cannot.
    pub unavailable: Option<String>,
    /// For each backfill, how many issues would be published, kept private and held back.
    pub counts: Vec<BackfillCount>,
    /// The first few issues as they would look on the host (with the backfill `open`).
    pub sample: Vec<Sample>,
    /// What went wrong publishing just now, when turning it on could not publish everything.
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackfillCount {
    pub backfill: String,
    pub publish: usize,
    pub private: usize,
    pub held: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub key: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// What turning the mirror on for `project` would publish, and whether it can.
pub fn preview(config: &Config, db: &Db, project: &str, check_host: bool) -> Result<Preview> {
    let project = db.project(project)?.with_context(|| format!("no project {project}"))?.key;
    let mut p = Preview { project: project.clone(), enabled: db.mirror_setting(&project)?, ..Default::default() };
    match host_for(config, &project) {
        Ok((host, _)) => {
            p.host = Some(host.name().into());
            p.repo = Some(host.repo().into());
            if check_host {
                if let Err(e) = host.check() {
                    p.unavailable = Some(format!("{e:#}"));
                }
            }
        }
        Err(e) => p.unavailable = Some(format!("{e:#}")),
    }
    let terms = match crate::private_terms::load() {
        Ok(t) => t,
        Err(e) => {
            // The card still shows whether the mirror is on; nothing is counted, and the sync
            // publishes nothing until the list reads again.
            p.unavailable = Some(format!("the private terms could not be read, so nothing would be checked before publishing: {e:#}"));
            return Ok(p);
        }
    };
    let roots = roots_of(config, &project);
    let issues = db.list_issues(Some(&project), true)?;
    for backfill in BACKFILLS {
        let mut c = BackfillCount { backfill: backfill.to_string(), publish: 0, private: 0, held: 0 };
        for i in issues.iter().filter(|i| match *backfill {
            "open" => i.is_open(),
            "all" => true,
            _ => false,
        }) {
            match decide(i, db.marked_private(&i.key)?, &terms, &roots)? {
                Decision::Publish(out) => {
                    c.publish += 1;
                    if *backfill == "open" && p.sample.len() < 3 {
                        p.sample.push(Sample { key: i.key.clone(), title: out.title, body: out.body, labels: out.labels });
                    }
                }
                Decision::Private(_) => c.private += 1,
                Decision::Held(_) => c.held += 1,
            }
        }
        p.counts.push(c);
    }
    Ok(p)
}

// ----- the sync -----

#[derive(Debug, Clone, Default, Serialize)]
pub struct MirrorReport {
    pub created: usize,
    pub updated: usize,
    pub withdrawn: usize,
    pub comments: usize,
    pub errors: Vec<String>,
}

/// Brings every mirroring project's host issues up to date, a bounded number of writes at a time.
/// The app runs it in the background, so a slow host never holds up filing an issue.
pub fn sync(config: &Config, db: &Db) -> MirrorReport {
    let mut report = MirrorReport::default();
    let projects: Vec<String> = match db.conn.prepare("SELECT project FROM mirror_project").and_then(|mut s| s.query_map([], |r| r.get(0))?.collect()) {
        Ok(p) => p,
        Err(_) => return report, // an older database without the mirror tables
    };
    if projects.is_empty() {
        return report;
    }
    // One sync at a time across processes (the app, the CLI), so nothing is published twice.
    let Some(_lock) = SyncLock::take() else { return report };
    // Without the private terms nothing can be checked, so nothing goes out.
    let terms = match crate::private_terms::load() {
        Ok(t) => t,
        Err(e) => {
            report.errors.push(format!("the private terms could not be read, so nothing was published: {e:#}"));
            return report;
        }
    };
    let mut budget = WRITES_PER_SYNC;
    for project in projects {
        let result = host_for(config, &project).and_then(|(host, _)| sync_project(db, host.as_ref(), &project, &terms, &roots_of(config, &project), &mut budget, &mut report));
        if let Err(e) = result {
            report.errors.push(format!("{project}: {e:#}"));
        }
    }
    report
}

/// One project's mirror, through `host`.
pub fn sync_project(db: &Db, host: &dyn Host, project: &str, terms: &PrivateTerms, roots: &[String], budget: &mut usize, report: &mut MirrorReport) -> Result<()> {
    let Some(setting) = db.mirror_setting(project)? else { return Ok(()) };
    for listed in db.list_issues(Some(project), true)? {
        if *budget == 0 {
            return Ok(());
        }
        // Read again right before acting on each: the mirror may have been turned off, or the issue
        // labelled security or given to the human, while earlier ones were being sent.
        let Some(setting) = db.mirror_setting(project)?.filter(|s| s.enabled_at == setting.enabled_at) else { return Ok(()) };
        let Some(issue) = db.issue(&listed.key)? else { continue };
        SyncLock::beat();
        // One issue the host refuses (deleted there, say) does not hold up the others.
        if let Err(e) = sync_issue(db, host, &issue, &setting, terms, roots, budget, report) {
            report.errors.push(format!("{}: {e:#}", issue.key));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn sync_issue(db: &Db, host: &dyn Host, issue: &IssueRecord, setting: &MirrorSetting, terms: &PrivateTerms, roots: &[String], budget: &mut usize, report: &mut MirrorReport) -> Result<()> {
    let known = db.issue_mirror(&issue.key)?;
    let created = crate::linear_import::parse_iso(&issue.created_at).unwrap_or(0);
    // A number counts only on the host and repo it was made on: after the origin moves, the
    // issue is published anew there, and nothing is written to the old repo's issue.
    let here = db.conn.query_row("SELECT host = ?2 AND repo = ?3 FROM issue_mirror WHERE issue_key = ?1", params![issue.key, host.name(), host.repo()], |r| r.get::<_, Option<bool>>(0)).optional()?.flatten().unwrap_or(false);
    let published = known.as_ref().filter(|_| here).and_then(|m| m.number.map(|n| (n, m.status.clone())));
    let eligible = published.is_some()
        || created >= setting.enabled_at
        || setting.backfill == "all"
        || (setting.backfill == "open" && issue.is_open());
    if !eligible {
        return Ok(());
    }
    match decide(issue, db.marked_private(&issue.key)?, terms, roots)? {
        Decision::Publish(out) => {
            let fingerprint = out.fingerprint();
            let unchanged = db.conn.query_row("SELECT fingerprint = ?2 AND status = 'mirrored' FROM issue_mirror WHERE issue_key = ?1", params![issue.key, fingerprint], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false);
            match published {
                None => {
                    // Created open, its number saved, then closed: a failed close is retried on
                    // the next sync instead of creating the issue again.
                    let open = HostIssue { closed: None, ..out.clone() };
                    let (number, url) = host.create(&open).with_context(|| format!("creating {}'s issue on {}", issue.key, host.name()))?;
                    *budget -= 1;
                    report.created += 1;
                    db.conn.execute("DELETE FROM mirror_comment WHERE issue_key = ?1", [&issue.key])?;
                    db.conn.execute("UPDATE issue_mirror SET host = NULL, repo = NULL, number = NULL, url = NULL WHERE issue_key = ?1", [&issue.key])?;
                    db.save_mirror(&issue.key, "mirrored", None, Some((host.name(), host.repo(), number, &url)), Some(&open.fingerprint()))?;
                    // Comments start from now: a backfill does not replay old history.
                    let last: i64 = db.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM event WHERE issue_key = ?1 COLLATE NOCASE", [&issue.key], |r| r.get(0))?;
                    db.conn.execute("UPDATE issue_mirror SET last_event = ?2 WHERE issue_key = ?1", params![issue.key, last])?;
                    if out.closed.is_some() && *budget > 0 {
                        host.update(number, &out).with_context(|| format!("closing {}'s issue on {}", issue.key, host.name()))?;
                        *budget -= 1;
                        db.save_mirror(&issue.key, "mirrored", None, None, Some(&fingerprint))?;
                    }
                }
                Some((number, _)) if !unchanged => {
                    host.update(number, &out).with_context(|| format!("updating {}'s issue on {}", issue.key, host.name()))?;
                    *budget -= 1;
                    report.updated += 1;
                    db.save_mirror(&issue.key, "mirrored", None, None, Some(&fingerprint))?;
                }
                Some(_) => {}
            }
            post_comments(db, host, issue, terms, roots, budget, report)?;
        }
        Decision::Private(why) | Decision::Held(why) if published.as_ref().is_some_and(|(_, s)| s == "mirrored") => {
            let (number, _) = published.unwrap();
            withdraw(db, host, &issue.key, number, &why)?;
            *budget = budget.saturating_sub(1);
            report.withdrawn += 1;
        }
        // Still public on the host (it could not be deleted): that stays said until it is.
        Decision::Private(_) | Decision::Held(_) if published.as_ref().is_some_and(|(_, s)| s == "exposed") => {}
        Decision::Private(why) => db.save_mirror(&issue.key, if published.is_some() { "withdrawn" } else { "private" }, Some(&why), None, None)?,
        Decision::Held(why) => db.save_mirror(&issue.key, if published.is_some() { "withdrawn" } else { "held" }, Some(&why), None, None)?,
    }
    Ok(())
}

/// Posts the issue's timeline entries an outsider can follow, since the last one posted. One with
/// a private term is skipped, as is everything an agent only reported.
fn post_comments(db: &Db, host: &dyn Host, issue: &IssueRecord, terms: &PrivateTerms, roots: &[String], budget: &mut usize, report: &mut MirrorReport) -> Result<()> {
    let (number, last): (Option<i64>, i64) = db.conn.query_row("SELECT number, last_event FROM issue_mirror WHERE issue_key = ?1", [&issue.key], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let Some(number) = number else { return Ok(()) };
    let mut stmt = db.conn.prepare("SELECT id, type, COALESCE(summary, '') FROM event WHERE issue_key = ?1 COLLATE NOCASE AND id > ?2 ORDER BY id")?;
    let events: Vec<(i64, String, String)> = stmt.query_map(params![issue.key, last], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let pr = db.prs_for_issue(&issue.key)?.into_iter().next().map(|p| p.url);
    let project = issue.project.as_deref().unwrap_or_default();
    for (id, kind, summary) in events {
        if *budget == 0 || db.mirror_setting(project)?.is_none() {
            return Ok(());
        }
        if COMMENTED.contains(&kind.as_str()) {
            if let Some(text) = comment_for(&kind, &summary, pr.as_deref(), roots) {
                if private_term_in(terms, project, &text)?.is_none() {
                    let id = host.comment(number, &text).with_context(|| format!("commenting on {}'s issue on {}", issue.key, host.name()))?;
                    db.conn.execute("INSERT OR IGNORE INTO mirror_comment (issue_key, host_id) VALUES (?1, ?2)", params![issue.key, id])?;
                    *budget -= 1;
                    report.comments += 1;
                }
            }
        }
        db.conn.execute("UPDATE issue_mirror SET last_event = ?2 WHERE issue_key = ?1", params![issue.key, id])?;
    }
    Ok(())
}

/// Takes a published issue down: its comments deleted, its title and text replaced with a note, and
/// closed as not planned.
fn withdraw(db: &Db, host: &dyn Host, key: &str, number: i64, why: &str) -> Result<()> {
    // Deleted outright where the token may: an edited text stays readable in the host's history.
    if host.delete_issue(number).is_ok() {
        db.conn.execute("DELETE FROM mirror_comment WHERE issue_key = ?1", [key])?;
        db.conn.execute("UPDATE issue_mirror SET number = NULL, url = NULL WHERE issue_key = ?1", [key])?;
        db.save_mirror(key, "withdrawn", Some(&format!("{why}; deleted on {}", host.name())), None, None)?;
        return Ok(());
    }
    let ids: Vec<i64> = db.conn.prepare("SELECT host_id FROM mirror_comment WHERE issue_key = ?1")?.query_map([key], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        host.delete_comment(number, id).with_context(|| format!("taking down a comment on {key}'s issue on {}", host.name()))?;
        db.conn.execute("DELETE FROM mirror_comment WHERE issue_key = ?1 AND host_id = ?2", params![key, id])?;
    }
    let gone = HostIssue {
        title: format!("{key} (withdrawn)"),
        body: "This issue was made private in Nitely, so its text and comments were taken down.".into(),
        labels: vec![],
        closed: Some("not_planned"),
        kind: String::new(),
        priority: 0,
    };
    host.update(number, &gone).with_context(|| format!("withdrawing {key}'s issue on {}", host.name()))?;
    // Not withdrawn: the earlier text is still readable in the host's edit history.
    let left = format!("{why}, but Nitely could not delete issue #{number} on {}: its earlier text is still readable in the issue's edit history, so delete the issue there", host.name());
    db.save_mirror(key, "exposed", Some(&left), None, Some(&gone.fingerprint()))?;
    bail!("{key}: {left}")
}

/// Withdraws `key` from its host now, if it was published there, whether or not its project still
/// mirrors: for "Keep private" after the mirror was turned off.
pub fn withdraw_now(config: &Config, db: &Db, key: &str) -> Result<()> {
    let Some(m) = db.issue_mirror(key)? else { return Ok(()) };
    let (Some(number), "mirrored") = (m.number, m.status.as_str()) else { return Ok(()) };
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    let (host, _) = host_for(config, issue.project.as_deref().unwrap_or_default())?;
    let (on, repo): (Option<String>, Option<String>) = db.conn.query_row("SELECT host, repo FROM issue_mirror WHERE issue_key = ?1", [&issue.key], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if on.as_deref() != Some(host.name()) || repo.as_deref() != Some(host.repo()) {
        bail!("{key} was published on {} {}, which is no longer this project's repo; take it down there", on.unwrap_or_default(), repo.unwrap_or_default());
    }
    let _lock = SyncLock::take().context("a mirror sync is running; try again in a moment")?;
    withdraw(db, host.as_ref(), &issue.key, number, "marked private")
}

/// The mirror's lock file in Nitely's data folder: one sync at a time across processes. One older
/// than ten minutes is taken to be left by a sync that died.
struct SyncLock(std::path::PathBuf);

impl SyncLock {
    fn take() -> Option<SyncLock> {
        let path = crate::paths::data_dir().join("mirror.lock");
        let stale = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 600);
        if stale {
            std::fs::remove_file(&path).ok();
        }
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path).ok().map(|_| SyncLock(path))
    }
}

impl SyncLock {
    /// Marks the lock fresh: a sync that keeps writing is never taken for a dead one.
    fn beat() {
        let path = crate::paths::data_dir().join("mirror.lock");
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
            f.set_modified(std::time::SystemTime::now()).ok();
        }
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// An issue's place in the mirror for the app: its record, or `unpublished` when its project's repo
/// has a tracker to mirror to (so the person can keep it private before anything goes out).
pub fn issue_state(config: &Config, db: &Db, key: &str) -> Result<Option<IssueMirror>> {
    if let Some(m) = db.issue_mirror(key)? {
        return Ok(Some(m));
    }
    let Some(issue) = db.issue(key)?.filter(|i| i.origin == "nitely") else { return Ok(None) };
    let project = issue.project.clone().unwrap_or_default();
    let host = config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&project)))
        .find_map(|r| crate::git::github_slug(&r.root).map(|_| "GitHub").or_else(|| crate::git::bitbucket_slug(&r.root).map(|_| "Bitbucket")));
    Ok(host.map(|h| IssueMirror { issue: issue.key, status: "unpublished".into(), host: Some(h.into()), ..Default::default() }))
}

/// The host issue a pull request for `key` closes, as `Closes #12`, when the issue is mirrored to
/// the tracker of the repo at `repo_root`.
pub fn closes_line(db: &Db, key: &str, repo_root: &Path) -> Option<String> {
    let m = db.issue_mirror(key).ok()??;
    let number = m.number.filter(|_| m.status == "mirrored")?;
    let root = repo_root.to_string_lossy();
    let slug = crate::git::github_slug(&root).or_else(|| crate::git::bitbucket_slug(&root))?;
    let repo: Option<String> = db.conn.query_row("SELECT repo FROM issue_mirror WHERE issue_key = ?1 COLLATE NOCASE", [key], |r| r.get(0)).ok()?;
    (repo.as_deref() == Some(slug.as_str())).then(|| format!("Closes #{number}"))
}

#[cfg(test)]
mod tests;
