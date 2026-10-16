//! Private terms: names, paths and patterns you keep out of a project's repos, such as another
//! project's name, a client or a local folder. The list lives in Nitely's data folder on this
//! machine only (listing the terms in a public repo would publish them). `submit` and `land` scan
//! what a branch adds, its added lines, new file paths and commit messages, and refuse it with
//! where each term was found. They never show the matched text or the list, so the list does
//! not reach an agent's context. Only the app reads and writes the list; no MCP tool or CLI
//! command shows it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::playbook::git;

/// One term per entry: plain text matches anywhere in a line, ignoring case, with `\` and `/`
/// treated alike; `re:` starts a regular expression. Blank entries and `#` comments are skipped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrivateTerms {
    /// Kept out of every project's repos.
    #[serde(default)]
    pub all: Vec<String>,
    /// Kept out of one project's repos, by project key.
    #[serde(default)]
    pub projects: BTreeMap<String, Vec<String>>,
}

/// Where a branch adds a private term. `term` is the entry's number in its list (1-based), so the
/// person can find it; the matched text itself is never reported.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    /// `src/app.ts:12`, `path src/acme/x.ts`, or `commit 1a2b3c4 message line 2`.
    pub at: String,
    /// `every project` or the project key.
    pub list: String,
    pub term: usize,
}

const FILE: &str = "private-terms.json";
/// Hits listed in a refusal; the rest are counted.
const SHOWN: usize = 30;

pub fn load() -> Result<PrivateTerms> {
    load_from(&crate::paths::data_dir())
}

pub fn load_from(data: &Path) -> Result<PrivateTerms> {
    let path = data.join(FILE);
    if !path.exists() {
        return Ok(PrivateTerms::default());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    // Not the parse error itself: serde's message can quote a term.
    serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("{} is not valid JSON", path.display()))
}

/// Saves the list after checking every pattern compiles. Empty lists are dropped.
pub fn save(terms: &PrivateTerms) -> Result<PrivateTerms> {
    save_in(&crate::paths::data_dir(), terms)
}

pub fn save_in(data: &Path, terms: &PrivateTerms) -> Result<PrivateTerms> {
    let mut clean = PrivateTerms { all: tidy(&terms.all), projects: BTreeMap::new() };
    for (key, list) in &terms.projects {
        let list = tidy(list);
        if !list.is_empty() {
            clean.projects.insert(key.trim().to_uppercase(), list);
        }
    }
    compile("every project", &clean.all)?;
    for (key, list) in &clean.projects {
        compile(key, list)?;
    }
    std::fs::create_dir_all(data).with_context(|| format!("creating {}", data.display()))?;
    let path = data.join(FILE);
    std::fs::write(&path, serde_json::to_string_pretty(&clean)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(clean)
}

/// Moves a project's list to its new key, or drops it with the project (`to` None).
pub fn retarget(from: &str, to: Option<&str>) -> Result<bool> {
    retarget_in(&crate::paths::data_dir(), from, to)
}

pub fn retarget_in(data: &Path, from: &str, to: Option<&str>) -> Result<bool> {
    let mut terms = load_from(data)?;
    let Some(list) = terms.projects.remove(&from.trim().to_uppercase()) else { return Ok(false) };
    if let Some(to) = to {
        terms.projects.entry(to.trim().to_uppercase()).or_default().extend(list);
    }
    save_in(data, &terms)?;
    Ok(true)
}

fn tidy(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in list.iter().map(|t| t.trim()) {
        if !t.is_empty() && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

enum Matcher {
    /// Lowercase, with `\` as `/`.
    Text(String),
    Pattern(regex::Regex),
}

impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::Text(t) => line.to_lowercase().replace('\\', "/").contains(t.as_str()),
            Matcher::Pattern(re) => re.is_match(line),
        }
    }
}

/// The list's terms, each with its number. An error names the entry, never its text.
fn compile(list: &str, terms: &[String]) -> Result<Vec<(usize, Matcher)>> {
    let mut out = Vec::new();
    for (i, term) in terms.iter().enumerate() {
        let term = term.trim();
        if term.is_empty() || term.starts_with('#') {
            continue;
        }
        let matcher = match term.strip_prefix("re:") {
            Some(p) => Matcher::Pattern(regex::Regex::new(p.trim()).map_err(|_| anyhow::anyhow!("private term {} of {list} is not a valid regular expression", i + 1))?),
            None => Matcher::Text(term.to_lowercase().replace('\\', "/")),
        };
        out.push((i + 1, matcher));
    }
    Ok(out)
}

/// What `branch` adds against `base` (a commit or branch): `(where, text)` for every added line,
/// every new or renamed file's path, and every line of the commit messages.
fn added(worktree: &Path, base: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let quiet = ["-c", "core.quotepath=false"];
    let paths = git(worktree, &[quiet[0], quiet[1], "diff", "--name-only", "--diff-filter=AR", &format!("{base}...HEAD")])?;
    out.extend(paths.lines().filter(|l| !l.is_empty()).map(|p| (format!("path {p}"), p.to_string())));

    let diff = git(worktree, &[quiet[0], quiet[1], "diff", "--no-color", "--no-ext-diff", "--unified=0", "--no-renames", &format!("{base}...HEAD")])?;
    let mut file: Option<String> = None;
    let mut line = 0usize;
    let mut in_hunk = false;
    for l in diff.lines() {
        if l.starts_with("diff --git ") {
            in_hunk = false;
            file = None;
        } else if !in_hunk && l.starts_with("+++ ") {
            file = l.strip_prefix("+++ b/").map(str::to_string);
        } else if let Some(rest) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@: added lines count from c.
            line = rest.split_whitespace().find_map(|p| p.strip_prefix('+')).and_then(|p| p.split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
            in_hunk = true;
        } else if in_hunk {
            if let Some(text) = l.strip_prefix('+') {
                if let Some(f) = &file {
                    out.push((format!("{f}:{line}"), text.to_string()));
                }
                line += 1;
            }
        }
    }

    let log = git(worktree, &["log", "--format=%h%x1f%B%x1e", &format!("{base}..HEAD")])?;
    for entry in log.split('\x1e') {
        let Some((hash, message)) = entry.trim_start().split_once('\x1f') else { continue };
        for (n, text) in message.lines().enumerate() {
            out.push((format!("commit {hash} message line {}", n + 1), text.to_string()));
        }
    }
    Ok(out)
}

/// Every place the branch adds one of `terms`, checking the project's list and the list for
/// every project.
pub fn scan(terms: &PrivateTerms, project: Option<&str>, worktree: &Path, base: &str) -> Result<Vec<Hit>> {
    let mut lists = vec![("every project".to_string(), compile("every project", &terms.all)?)];
    if let Some(key) = project.map(|p| p.trim().to_uppercase()) {
        if let Some(list) = terms.projects.get(&key) {
            lists.push((key.clone(), compile(&key, list)?));
        }
    }
    if lists.iter().all(|(_, m)| m.is_empty()) {
        return Ok(vec![]);
    }
    let mut hits = Vec::new();
    for (at, text) in added(worktree, base)? {
        for (list, matchers) in &lists {
            for (n, m) in matchers {
                if m.matches(&text) {
                    hits.push(Hit { at: at.clone(), list: list.clone(), term: *n });
                }
            }
        }
    }
    Ok(hits)
}

/// Refuses a branch that adds a private term, saying where and which entry, never the text.
pub fn check(project: Option<&str>, worktree: &Path, base: &str) -> Result<()> {
    check_with(&load()?, project, worktree, base)
}

pub fn check_with(terms: &PrivateTerms, project: Option<&str>, worktree: &Path, base: &str) -> Result<()> {
    let hits = scan(terms, project, worktree, base)?;
    if hits.is_empty() {
        return Ok(());
    }
    let mut msg = format!(
        "the branch adds {} you keep out of this repo. Remove {} (and from commit messages: reword or squash those commits), then try again. \
         The list is in Nitely's project settings; it is not shown here.",
        if hits.len() == 1 { "a private term".to_string() } else { format!("private terms in {} places", hits.len()) },
        if hits.len() == 1 { "it" } else { "them" },
    );
    for h in hits.iter().take(SHOWN) {
        let list = if h.list == "every project" { "the list for every project".to_string() } else { format!("{}'s list", h.list) };
        msg.push_str(&format!("\n  {}: term {} of {list}", h.at, h.term));
    }
    if hits.len() > SHOWN {
        msg.push_str(&format!("\n  and {} more", hits.len() - SHOWN));
    }
    bail!(msg)
}

/// The data folder's list file, for the app to say where it is kept.
pub fn path() -> PathBuf {
    crate::paths::data_dir().join(FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn terms(all: &[&str], app: &[&str]) -> PrivateTerms {
        PrivateTerms {
            all: all.iter().map(|s| s.to_string()).collect(),
            projects: BTreeMap::from([("APP".to_string(), app.iter().map(|s| s.to_string()).collect())]),
        }
    }

    #[test]
    fn finds_added_lines_paths_and_messages_and_never_echoes_the_term() {
        let dir = std::env::temp_dir().join(format!("nitely-terms-{}-{}", std::process::id(), crate::db::now()));
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "old line mentions Bluebird already\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "init"]);
        sh(&dir, &["checkout", "-q", "-b", "claude/app-1-x"]);
        std::fs::write(dir.join("a.txt"), "old line mentions Bluebird already\nnew line\nsee C:\\Work\\Secret-Client\\notes\n").unwrap();
        std::fs::create_dir_all(dir.join("bluebird")).unwrap();
        std::fs::write(dir.join("bluebird/b.txt"), "clean\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "Port the change\n\nas in the BLUEBIRD repo"]);

        let none = scan(&PrivateTerms::default(), Some("APP"), &dir, "main").unwrap();
        assert!(none.is_empty(), "no list, nothing to find");

        let t = terms(&["bluebird"], &["c:/work/secret-client", "re:ticket-\\d+"]);
        let hits = scan(&t, Some("app"), &dir, "main").unwrap();
        let at: Vec<&str> = hits.iter().map(|h| h.at.as_str()).collect();
        assert!(at.contains(&"path bluebird/b.txt"), "{at:?}");
        assert!(at.contains(&"a.txt:3"), "a path written with backslashes: {at:?}");
        assert!(at.iter().any(|a| a.starts_with("commit ") && a.ends_with("message line 3")), "{at:?}");
        assert!(!at.contains(&"a.txt:1"), "a line the branch did not add: {at:?}");
        assert_eq!(hits.iter().find(|h| h.at == "a.txt:3").map(|h| (h.list.as_str(), h.term)), Some(("APP", 1)));
        assert!(scan(&t, Some("OTHER"), &dir, "main").unwrap().iter().all(|h| h.list == "every project"), "another project's list does not apply");

        let err = check_with(&t, Some("APP"), &dir, "main").unwrap_err().to_string();
        assert!(err.contains("a.txt:3: term 1 of APP's list"), "{err}");
        assert!(!err.to_lowercase().contains("bluebird") || err.contains("path bluebird/"), "the term shows only as part of a path it is in: {err}");
        assert!(!err.to_lowercase().contains("secret-client"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saves_tidily_refuses_bad_patterns_and_follows_a_new_key() {
        let data = std::env::temp_dir().join(format!("nitely-terms-data-{}-{}", std::process::id(), crate::db::now()));
        let saved = save_in(&data, &PrivateTerms {
            all: vec![" Bluebird ".into(), "".into(), "Bluebird".into()],
            projects: BTreeMap::from([("app".to_string(), vec!["x".to_string()]), ("WEB".to_string(), vec![" ".to_string()])]),
        })
        .unwrap();
        assert_eq!(saved.all, ["Bluebird"]);
        assert_eq!(saved.projects.keys().collect::<Vec<_>>(), ["APP"], "keys uppercase, empty lists dropped");
        assert_eq!(load_from(&data).unwrap(), saved);

        let err = save_in(&data, &terms(&[], &["fine", "re:(unclosed"])).unwrap_err().to_string();
        assert!(err.contains("term 2 of APP") && !err.contains("unclosed"), "{err}");
        assert_eq!(load_from(&data).unwrap(), saved, "a refused save changes nothing");

        assert!(retarget_in(&data, "APP", Some("AP")).unwrap());
        assert_eq!(load_from(&data).unwrap().projects.keys().collect::<Vec<_>>(), ["AP"]);
        assert!(retarget_in(&data, "AP", None).unwrap());
        assert!(load_from(&data).unwrap().projects.is_empty());
        assert!(!retarget_in(&data, "NONE", None).unwrap());
        std::fs::remove_dir_all(&data).ok();
    }
}
