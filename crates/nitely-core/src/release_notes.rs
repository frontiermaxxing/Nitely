//! Release notes from the board: the issues that landed between two commits, with their titles and
//! a link to their pull requests. A land keeps each commit, and commit messages name their issue
//! (`… (NIT-80)` in the subject, or a `nitely://issue/NIT-80` link in the body), so the commits
//! since the last release say which issues it carries. The
//! title comes from the issue when this machine has it, else from its first commit, so the notes
//! read the same in CI, where there is no database.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

use crate::db::Db;
use crate::playbook::git;

/// One issue in a release: its key, title and the commits that carried it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub title: String,
    pub commits: usize,
}

/// The issue a commit names: the last `(KEY-123)` in its subject, else the first issue link in its
/// body (`nitely://issue/KEY-123` or `http://127.0.0.1:7423/i/KEY-123`).
pub fn key_of(subject: &str, body: &str) -> Option<String> {
    let in_subject = regex::Regex::new(r"\(([A-Z][A-Z0-9]{1,9}-\d+)\)").expect("valid");
    let linked = regex::Regex::new(r"(?:nitely://issue/|http://(?:127\.0\.0\.1|localhost):\d+/i/)([A-Z][A-Z0-9]{1,9}-\d+)").expect("valid");
    in_subject
        .captures_iter(subject)
        .last()
        .or_else(|| linked.captures(body))
        .map(|c| c[1].to_string())
}

/// The issues in `since..until` (oldest first), and the subjects of commits that name none.
pub fn entries(repo: &Path, since: Option<&str>, until: &str, db: Option<&Db>) -> Result<(Vec<Entry>, Vec<String>)> {
    let range = since.map_or_else(|| until.to_string(), |s| format!("{s}..{until}"));
    let log = git(repo, &["log", "--reverse", "--no-merges", "--format=%s%x00%b%x01", &range])?;
    let mut order: Vec<String> = Vec::new();
    let mut by_key: BTreeMap<String, Entry> = BTreeMap::new();
    let mut other = Vec::new();
    for commit in log.split('\x01').map(str::trim_start).filter(|c| !c.is_empty()) {
        let (subject, body) = commit.split_once('\0').unwrap_or((commit, ""));
        let Some(key) = key_of(subject, body) else {
            other.push(subject.to_string());
            continue;
        };
        let entry = by_key.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            let title = db
                .and_then(|db| db.issue(&key).ok().flatten())
                .map(|i| i.title)
                .unwrap_or_else(|| strip_key(subject, &key));
            Entry { key: key.clone(), title, commits: 0 }
        });
        entry.commits += 1;
    }
    Ok((order.into_iter().filter_map(|k| by_key.remove(&k)).collect(), other))
}

fn strip_key(subject: &str, key: &str) -> String {
    subject.replace(&format!("({key})"), "").trim().trim_end_matches([':', '-']).trim().to_string()
}

/// The notes for the first release, which has no previous one to count from. They list nothing:
/// the history before it holds commits written before Nitely checked them for private terms
/// (NIT-80), and release notes are public.
pub fn first_release(version: &str) -> String {
    format!(
        "## Nitely {version}\n\nThe first release of Nitely: one person running a team of coding agents across repos and vendors. \
         The README says what it does and how to start.\n"
    )
}

/// The notes as Markdown: a heading, one line per issue with a link to its pull requests on
/// GitHub (`repo` is `owner/name`), and the commits that name no issue.
pub fn markdown(version: &str, entries: &[Entry], other: &[String], repo: Option<&str>) -> String {
    let mut out = format!("## Nitely {version}\n\n");
    if entries.is_empty() && other.is_empty() {
        out.push_str("No changes since the previous release.\n");
        return out;
    }
    if !entries.is_empty() {
        out.push_str("What landed:\n\n");
        for e in entries {
            let link = repo.map(|r| format!(" ([pull request](https://github.com/{r}/pulls?q=is%3Apr+{}+in%3Atitle))", e.key)).unwrap_or_default();
            out.push_str(&format!("- **{}** {}{link}\n", e.key, e.title));
        }
    }
    if !other.is_empty() {
        out.push_str(if entries.is_empty() { "Changes:\n\n" } else { "\nOther changes:\n\n" });
        for s in other {
            out.push_str(&format!("- {s}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn keys_titles_and_the_commits_that_name_none() {
        assert_eq!(key_of("Fix the thing (APP-12)", "").as_deref(), Some("APP-12"));
        assert_eq!(key_of("Follow (APP-3) up (APP-12)", "").as_deref(), Some("APP-12"));
        assert_eq!(key_of("Add goals", "APP-40 (nitely://issue/APP-40), slice of APP-36").as_deref(), Some("APP-40"));
        assert_eq!(key_of("Add goals", "[APP-41](http://127.0.0.1:7423/i/APP-41)").as_deref(), Some("APP-41"));
        assert_eq!(key_of("Use UTF-8 everywhere", "No link"), None);

        let dir = crate::paths::unique_temp("nitely-notes", "");
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success());
        sh(&["init", "-q", "-b", "main"]);
        sh(&["commit", "-q", "--allow-empty", "-m", "Start"]);
        sh(&["tag", "v0.1.0"]);
        for m in ["Add the login page (APP-2)", "Bump a dependency", "Fix review findings (APP-2)", "Show the price twice no more (APP-5)"] {
            sh(&["commit", "-q", "--allow-empty", "-m", m]);
        }
        sh(&["commit", "-q", "--allow-empty", "-m", "Add goals", "-m", "[APP-7](nitely://issue/APP-7)"]);
        let (entries, other) = entries(&dir, Some("v0.1.0"), "HEAD", None).unwrap();
        assert_eq!(entries.iter().map(|e| (e.key.as_str(), e.title.as_str(), e.commits)).collect::<Vec<_>>(), [("APP-2", "Add the login page", 2), ("APP-5", "Show the price twice no more", 1), ("APP-7", "Add goals", 1)]);
        assert_eq!(other, ["Bump a dependency"]);

        let md = markdown("0.2.0", &entries, &other, Some("acme/app"));
        assert!(md.starts_with("## Nitely 0.2.0\n"), "{md}");
        assert!(md.contains("- **APP-2** Add the login page ([pull request](https://github.com/acme/app/pulls?q=is%3Apr+APP-2+in%3Atitle))"), "{md}");
        assert!(md.contains("Other changes:\n\n- Bump a dependency"), "{md}");
        assert!(markdown("0.2.1", &[], &[], None).contains("No changes"));
        let first = first_release("0.1.0");
        assert!(first.starts_with("## Nitely 0.1.0\n") && !first.contains("- **"), "the first release lists no history: {first}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
