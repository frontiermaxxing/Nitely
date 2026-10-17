//! Changing projects after they exist: a new name, a new key (the issue prefix), or removing one.
//!
//! A new key re-keys every issue of the project (NIT-12 becomes NX-12) and every row that names
//! one, in one transaction. The old key stays behind as an alias, so a `nitely://issue/NIT-12`
//! link, a lookup by the old key and an old branch (`claude/nit-12-...`) still find the issue.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::db::now;
use crate::Db;

/// Every column that holds an issue key, so a re-key rewrites them all.
const KEY_COLUMNS: &[(&str, &str)] = &[
    ("issue", "key"),
    ("issue_blocker", "issue_key"),
    ("issue_blocker", "blocker_key"),
    ("task", "issue_key"),
    ("event", "issue_key"),
    ("review", "issue_key"),
    ("approval", "issue_key"),
    ("accepted_finding", "issue_key"),
    ("ask", "issue_key"),
    ("crew_worker", "issue_key"),
    ("playbook_proposal", "issue_key"),
    ("goal_plan", "issue_key"),
    ("issue_mirror", "issue_key"),
    ("issue_private", "issue_key"),
    ("mirror_comment", "issue_key"),
    ("submit_job", "issue_key"),
];

/// `key` under its project's current prefix, given `Db::project_aliases`: `OLD-12` → `NEW-12`.
pub fn current_key(aliases: &HashMap<String, String>, key: &str) -> String {
    match key.split_once('-') {
        Some((prefix, number)) => match aliases.get(&prefix.to_uppercase()) {
            Some(project) => format!("{project}-{number}"),
            None => key.to_string(),
        },
        None => key.to_string(),
    }
}

/// A project key: 2-10 letters or digits, starting with a letter, upper-cased.
pub fn valid_key(key: &str) -> Result<String> {
    let key = key.trim().to_uppercase();
    let valid = (2..=10).contains(&key.len())
        && key.starts_with(|c: char| c.is_ascii_uppercase())
        && key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !valid {
        bail!("project key must be 2-10 letters or digits, starting with a letter (e.g. NIT)");
    }
    Ok(key)
}

#[derive(Debug, Clone, Serialize)]
pub struct Rekeyed {
    pub from: String,
    pub to: String,
    /// Issues that got the new key.
    pub issues: i64,
    /// Repos in the config, and machines allowed through `nitely serve`, that now name the new key.
    pub repos: usize,
    pub machines: usize,
    /// What could not follow (a config file that would not save). The old key still reaches the
    /// project there, through its alias.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    pub key: String,
    /// Issues deleted with it (only when asked to).
    pub issues: i64,
    /// Repos and machines that named it and now name no project.
    pub repos: usize,
    pub machines: usize,
    pub warnings: Vec<String>,
}

/// Re-keys a project: refused while a worktree has one of its issues checked out; then the
/// database, then the repos and machine tokens that name it. The database is the change; the
/// config and tokens follow as best they can, and an old key left in them still works.
pub fn rekey(db: &mut Db, from: &str, to: &str) -> Result<Rekeyed> {
    let config = crate::Config::load()?;
    let to_key = valid_key(to)?;
    let from_key = from.trim().to_uppercase();
    // Run again after the database changed but a config or token file did not: finish those.
    if db.project_exact(&from_key)?.is_none() && db.project_aliases()?.get(&from_key) == Some(&to_key) {
        let (repos, machines, warnings) = follow(config, &from_key, Some(&to_key));
        return Ok(Rekeyed { from: from_key, to: to_key, issues: 0, repos, machines, warnings });
    }
    let key = db.project_exact(from)?.with_context(|| format!("no project {from}"))?.key;
    let busy = checked_out(&config, db, &key);
    if !busy.is_empty() {
        bail!("{}", checked_out_message(&key, &busy));
    }
    let mut done = db.rekey_project(&key, to)?;
    let (repos, machines, warnings) = follow(config, &done.from, Some(&done.to));
    (done.repos, done.machines, done.warnings) = (repos, machines, warnings);
    Ok(done)
}

/// Removes a project (see `Db::remove_project`), refused while a worktree has one of its issues
/// checked out; repos and machine tokens that named it then name none.
pub fn remove(db: &mut Db, key: &str, with_issues: bool) -> Result<Removed> {
    let config = crate::Config::load()?;
    let wanted = key.trim().to_uppercase();
    // Run again after the project went but a config or token file still names it: finish those.
    if db.project(&wanted)?.is_none() {
        let named = config.repos.iter().any(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&wanted)))
            || crate::relay::list_tokens()?.iter().any(|t| t.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&wanted)));
        if named {
            let (repos, machines, warnings) = follow(config, &wanted, None);
            return Ok(Removed { key: wanted, issues: 0, repos, machines, warnings });
        }
    }
    let key = db.project_exact(key)?.with_context(|| format!("no project {key}"))?.key;
    let busy = checked_out(&config, db, &key);
    if !busy.is_empty() {
        bail!("{}", checked_out_message(&key, &busy));
    }
    let folders = folders_of(&config, db, &key);
    let mut done = db.remove_project(&key, with_issues)?;
    // Nothing of it stays live: its agents' sessions end and the questions they asked go, so no
    // view keeps showing it as working or waiting on you (NIT-189).
    let mut warnings = Vec::new();
    for folder in &folders {
        let why = format!("Ended: project {key} was removed");
        if let Err(e) = db.end_sessions_in(folder, 0, None, now(), &why).and_then(|_| db.remove_open_asks_in(folder)) {
            warnings.push(format!("sessions or questions in {folder} were left as they were: {e:#}"));
        }
    }
    let (repos, machines, more) = follow(config, &done.key, None);
    warnings.extend(more);
    (done.repos, done.machines, done.warnings) = (repos, machines, warnings);
    Ok(done)
}

/// The folders only this project's agents work in: each of its repos' main checkout and the
/// worktrees git lists for it, and in the playbook's worktree folder the task folders named for one
/// of its issues (`app-3-fix-login` for APP-3) that a session or question still names, gone or
/// not. The worktree folder itself is not one: it may be shared with other repos (`../{key}-{slug}`
/// puts every task beside the repos).
fn folders_of(config: &crate::Config, db: &Db, project: &str) -> Vec<String> {
    // A task folder starts with one of the project's keys, now or before a re-key, and a number
    // (`app2-14-...` for APP2-14); keys may hold digits, so it is matched against them directly.
    let mut keys: Vec<String> = vec![project.to_lowercase()];
    keys.extend(db.project_aliases().unwrap_or_default().into_iter().filter(|(_, now)| now.eq_ignore_ascii_case(project)).map(|(was, _)| was.to_lowercase()));
    let mine = |name: &str| {
        let name = name.to_lowercase();
        // The issue's number, then the slug's dash or the end: `app-2fa` is another repo, not APP-2.
        keys.iter().any(|k| {
            name.strip_prefix(&format!("{k}-")).is_some_and(|rest| {
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                digits > 0 && matches!(rest[digits..].chars().next(), None | Some('-'))
            })
        })
    };
    let named: Vec<String> = {
        let rows = db.conn.prepare("SELECT cwd FROM session WHERE state != 'ended' UNION SELECT cwd FROM ask WHERE answered_at IS NULL AND cwd IS NOT NULL").and_then(|mut stmt| {
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
        });
        rows.unwrap_or_default().iter().map(|c| crate::paths::normalize(c)).collect()
    };
    let mut out = Vec::new();
    for r in config.repos.iter().filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))) {
        out.push(r.root.clone());
        out.extend(crate::git::list_worktrees(&r.root).unwrap_or_default().into_iter().filter(|w| !w.is_main).map(|w| w.path));
        let root = Path::new(&r.root);
        let Some(folder) = crate::playbook::Playbook::load_with(root, config).ok().and_then(|pb| pb.worktrees_folder(root)) else { continue };
        let folder = crate::paths::normalize(&folder.to_string_lossy());
        for cwd in &named {
            let Some(task) = cwd.strip_prefix(&format!("{folder}/")).and_then(|rest| rest.split('/').next()) else { continue };
            let dir = format!("{folder}/{task}");
            if mine(task) && !out.iter().any(|d| crate::paths::normalize(d) == dir) {
                out.push(dir);
            }
        }
    }
    out
}

/// Points the config's repos and the machine tokens that name `from` at `to` (or at none).
fn follow(mut config: crate::Config, from: &str, to: Option<&str>) -> (usize, usize, Vec<String>) {
    let mut warnings = vec![];
    let repos = config.retarget_project(from, to);
    if repos > 0 {
        if let Err(e) = config.save() {
            warnings.push(format!("repos still name {from} in the config: {e:#}"));
        }
    }
    let machines = crate::relay::retarget_project(from, to).unwrap_or_else(|e| {
        warnings.push(format!("machine tokens still name {from}: {e:#}"));
        0
    });
    if let Err(e) = crate::private_terms::retarget(from, to) {
        warnings.push(format!("the private terms kept for {from} did not follow: {e:#}"));
    }
    (repos, machines, warnings)
}

impl Db {
    /// Old project keys and the project each now belongs to.
    pub fn project_aliases(&self) -> Result<HashMap<String, String>> {
        let mut stmt = match self.conn.prepare("SELECT old_key, project FROM project_alias") {
            Ok(s) => s,
            // A database opened read-only before it was migrated has no aliases yet.
            Err(_) => return Ok(HashMap::new()),
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `key` under its project's current prefix when the prefix is an old one, else None.
    pub fn alias_of(&self, key: &str) -> Result<Option<String>> {
        let Some((prefix, number)) = key.split_once('-') else { return Ok(None) };
        let project: Option<String> = match self.conn.query_row(
            "SELECT project FROM project_alias WHERE old_key = ?1 COLLATE NOCASE",
            [prefix],
            |r| r.get(0),
        ) {
            Ok(p) => Some(p),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            // See project_aliases.
            Err(_) => None,
        };
        Ok(project.map(|p| format!("{p}-{number}")))
    }

    /// `key` as it is now: under the current prefix when it names an old one (`OLD-12` → `NEW-12`).
    pub fn current_key(&self, key: &str) -> Result<String> {
        Ok(self.alias_of(key.trim())?.unwrap_or_else(|| key.trim().to_string()))
    }

    pub fn rename_project(&self, key: &str, name: &str) -> Result<crate::issues::Project> {
        let project = self.project_exact(key)?.with_context(|| format!("no project {key}"))?;
        let name = name.trim();
        if name.is_empty() {
            bail!("a project needs a name");
        }
        self.conn.execute("UPDATE project SET name = ?2 WHERE key = ?1", params![project.key, name])?;
        self.project(&project.key)?.context("project vanished after rename")
    }

    /// Gives a project a new key, and every one of its issues the new prefix. Only through
    /// `projects::rekey`, which first refuses while a worktree's branch names one of its issues.
    ///
    /// Refused when issues with the prefix are mirrored from another tracker (their keys are the
    /// tracker's, not Nitely's), and while one is in an open task row. Every check and change runs in one immediate
    /// transaction, so an issue filed meanwhile is either re-keyed with the rest or refused.
    pub(crate) fn rekey_project(&mut self, from: &str, to: &str) -> Result<Rekeyed> {
        let to = valid_key(to)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let from: String = tx
            .query_row("SELECT key FROM project WHERE key = ?1 COLLATE NOCASE", [from.trim()], |r| r.get(0))
            .optional()?
            .with_context(|| format!("no project {from}"))?;
        if to == from {
            bail!("{from} already has that key");
        }
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM project WHERE key = ?1)
                 OR EXISTS (SELECT 1 FROM issue WHERE key LIKE ?1 || '-%')",
            [&to],
            |r| r.get(0),
        )?;
        if taken {
            bail!("{to} is already a project or an issue prefix");
        }
        // An old key can come back to the project that had it, but not go to another one, or its
        // old links would open the wrong issues.
        let earlier: Option<String> =
            tx.query_row("SELECT project FROM project_alias WHERE old_key = ?1", [&to], |r| r.get(0)).optional()?;
        if let Some(p) = earlier.filter(|p| *p != from) {
            bail!("{to} was an earlier key of {p}, and its old links still lead there");
        }
        let mirrored: i64 = tx.query_row(
            "SELECT COUNT(*) FROM issue WHERE key LIKE ?1 || '-%' AND origin <> 'nitely'",
            [&from],
            |r| r.get(0),
        )?;
        if mirrored > 0 {
            bail!("{mirrored} issue(s) with the prefix {from} are mirrored from another tracker, which owns their keys");
        }
        let open: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT issue_key FROM task WHERE closed_at IS NULL AND issue_key LIKE ?1 || '-%' ORDER BY issue_key")?;
            let rows = stmt.query_map([&from], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if !open.is_empty() {
            bail!("{}", checked_out_message(&from, &open));
        }
        let issues: i64 = tx.query_row("SELECT COUNT(*) FROM issue WHERE project = ?1", [&from], |r| r.get(0))?;
        for (table, column) in KEY_COLUMNS {
            tx.execute(
                &format!("UPDATE {table} SET {column} = ?2 || substr({column}, length(?1) + 1) WHERE {column} LIKE ?1 || '-%'"),
                params![from, to],
            )?;
        }
        tx.execute("UPDATE issue SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("UPDATE project SET key = ?2 WHERE key = ?1", params![from, to])?;
        tx.execute("UPDATE project_alias SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("UPDATE project_brief SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("UPDATE goal SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("UPDATE goal_plan SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("UPDATE crew_worker SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("UPDATE mirror_project SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("DELETE FROM crew_control WHERE project = ?1", [&to])?;
        tx.execute("UPDATE crew_control SET project = ?2 WHERE project = ?1", params![from, to])?;
        tx.execute("DELETE FROM project_alias WHERE old_key = ?1", [&to])?;
        tx.execute(
            "INSERT OR REPLACE INTO project_alias (old_key, project, created_at) VALUES (?1, ?2, ?3)",
            params![from, to, now()],
        )?;
        tx.commit()?;
        Ok(Rekeyed { from, to, issues, repos: 0, machines: 0, warnings: vec![] })
    }

    /// Removes a project. Only through `projects::remove`, which first refuses while a worktree's
    /// branch names one of its issues. Refused while it has issues, unless `with_issues`, which
    /// deletes them and everything recorded about them, and while one is in an open task row. The count and the delete share one immediate transaction, so an issue filed
    /// meanwhile is never deleted without consent.
    pub(crate) fn remove_project(&mut self, key: &str, with_issues: bool) -> Result<Removed> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key: String = tx
            .query_row("SELECT key FROM project WHERE key = ?1 COLLATE NOCASE", [key.trim()], |r| r.get(0))
            .optional()?
            .with_context(|| format!("no project {key}"))?;
        let issues: i64 = tx.query_row("SELECT COUNT(*) FROM issue WHERE project = ?1", [&key], |r| r.get(0))?;
        if issues > 0 && !with_issues {
            bail!("{key} has {issues} issue(s); remove it with its issues, or move them first");
        }
        let open: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT issue_key FROM task WHERE closed_at IS NULL AND issue_key IN (SELECT key FROM issue WHERE project = ?1)
                 ORDER BY issue_key",
            )?;
            let rows = stmt.query_map([&key], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if !open.is_empty() {
            bail!("{}", checked_out_message(&key, &open));
        }
        let of_project = "(SELECT key FROM issue WHERE project = ?1)";
        for (table, column) in KEY_COLUMNS.iter().filter(|(t, _)| *t != "issue") {
            tx.execute(&format!("DELETE FROM {table} WHERE {column} IN {of_project}"), [&key])?;
        }
        tx.execute("DELETE FROM issue WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM project_alias WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM project_brief WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM goal WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM crew_control WHERE project = ?1", [&key])?;
        // A project made again under this key starts with the mirror off and nothing published.
        tx.execute("DELETE FROM mirror_project WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM project WHERE key = ?1", [&key])?;
        tx.commit()?;
        Ok(Removed { key, issues, repos: 0, machines: 0, warnings: vec![] })
    }
}

/// "NIT-3 and NIT-7 are checked out ...": why a project cannot change while work is in flight.
fn checked_out_message(key: &str, issues: &[String]) -> String {
    let one = issues.len() == 1;
    format!(
        "{} {} checked out in a worktree whose branch names {key}; land or close {} first",
        issues.join(", "),
        if one { "is" } else { "are" },
        if one { "it" } else { "them" }
    )
}

/// The issues of `project` that a worktree of a configured repo has checked out (its branch
/// names them). Those branches would keep the old key, so neither a re-key nor a removal may run.
fn checked_out(config: &crate::Config, db: &Db, project: &str) -> Vec<String> {
    let aliases = db.project_aliases().unwrap_or_default();
    let mut keys: Vec<String> = config
        .repos
        .iter()
        .flat_map(|repo| crate::git::list_worktrees(&repo.root).unwrap_or_default())
        .filter_map(|wt| wt.branch.as_deref().and_then(|b| crate::git::parse_branch(b).1))
        .map(|k| current_key(&aliases, &k))
        .filter(|k| k.split_once('-').is_some_and(|(p, _)| p.eq_ignore_ascii_case(project)))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::{Actor, IssueRecord, NewIssue};

    fn db() -> Db {
        let dir = crate::paths::unique_temp("nitely-projects", "");
        Db::open(&dir.join("t.db")).unwrap()
    }


    fn issue(db: &mut Db, project: &str, title: &str, blocked_by: Vec<String>) -> String {
        let new = NewIssue {
            project: project.into(),
            title: title.into(),
            body: "why".into(),
            evidence: "none yet".into(),
            kind: "bug".into(),
            source: "human".into(),
            blocked_by,
            ..Default::default()
        };
        db.create_issue(&new, &Actor::Human).unwrap().key
    }

    #[test]
    fn rekey_moves_every_issue_and_keeps_old_keys_working() {
        let mut db = db();
        db.create_project("OLD", "Old").unwrap();
        let a = issue(&mut db, "OLD", "First", vec![]);
        let b = issue(&mut db, "OLD", "Second", vec![a.clone()]);
        assert_eq!((a.as_str(), b.as_str()), ("OLD-1", "OLD-2"));

        let r = db.rekey_project("old", "new").unwrap();
        assert_eq!((r.from.as_str(), r.to.as_str(), r.issues), ("OLD", "NEW", 2));
        assert!(db.project_exact("OLD").unwrap().is_none());
        assert_eq!(db.project("old").unwrap().unwrap().key, "NEW", "an old key still reaches the project");
        let moved = db.issue("NEW-2").unwrap().unwrap();
        assert_eq!(moved.project.as_deref(), Some("NEW"));
        assert_eq!(moved.blocked_by, vec!["NEW-1".to_string()]);
        assert!(!db.timeline("NEW-1").unwrap().is_empty(), "the timeline follows the issue");
        // Old keys and links still find it.
        assert_eq!(db.issue("old-2").unwrap().unwrap().key, "NEW-2");
        assert_eq!(db.alias_of("OLD-7").unwrap().as_deref(), Some("NEW-7"));
        assert_eq!(db.current_key("old-7").unwrap(), "NEW-7");
        // Writes take old keys too (MCP update_issue always passes through change_blockers).
        db.change_blockers("OLD-2", &[], &["OLD-1".to_string()], &Actor::Human).unwrap();
        assert!(db.issue("NEW-2").unwrap().unwrap().blocked_by.is_empty());
        db.change_blockers("old-2", &["old-1".to_string()], &[], &Actor::Human).unwrap();
        assert_eq!(db.issue("NEW-2").unwrap().unwrap().blocked_by, vec!["NEW-1".to_string()]);
        // New issues continue the numbering under the new key.
        assert_eq!(issue(&mut db, "NEW", "Third", vec![]), "NEW-3");
        // The old key cannot be given to another project, but can come back to this one.
        db.create_project("OTH", "Other").unwrap();
        assert!(db.rekey_project("OTH", "OLD").is_err());
        db.rekey_project("NEW", "OLD").unwrap();
        assert_eq!(db.issue("NEW-1").unwrap().unwrap().key, "OLD-1");
    }

    #[test]
    fn a_tracker_does_not_mirror_issues_back_under_an_old_key() {
        let mut db = db();
        db.create_project("APP", "Acme").unwrap();
        issue(&mut db, "APP", "Native", vec![]);
        db.rekey_project("APP", "AP").unwrap();
        // Linear still has APP-1 and an unrelated LIN-1.
        let mirrored = |key: &str| IssueRecord { key: key.into(), origin: "linear".into(), title: "From Linear".into(), state: "Todo".into(), ..Default::default() };
        db.replace_issues("linear", &[mirrored("APP-1"), mirrored("LIN-1")]).unwrap();
        let old = db.issue("APP-1").unwrap().unwrap();
        assert_eq!((old.key.as_str(), old.origin.as_str()), ("AP-1", "nitely"), "the old link opens the native issue");
        assert!(db.issue("LIN-1").unwrap().is_some(), "other prefixes still mirror");
        // Even a mirror row left from before (written past replace_issues) does not take the old link.
        db.conn.execute("INSERT INTO issue (key, origin, title, state) VALUES ('APP-1', 'linear', 'Stale mirror', 'Todo')", []).unwrap();
        assert_eq!(db.issue("APP-1").unwrap().unwrap().key, "AP-1");
    }

    #[test]
    fn rekey_refuses_bad_or_taken_keys_and_open_work() {
        let mut db = db();
        db.create_project("AAA", "A").unwrap();
        db.create_project("BBB", "B").unwrap();
        assert!(db.rekey_project("AAA", "BBB").is_err(), "taken");
        assert!(db.rekey_project("AAA", "1X").is_err(), "invalid");
        assert!(db.rekey_project("ZZZ", "CCC").is_err(), "no such project");
        let key = issue(&mut db, "AAA", "Busy", vec![]);
        db.conn
            .execute(
                "INSERT INTO task (issue_key, repo, branch, created_at) VALUES (?1, 'r', 'claude/aaa-1-busy', 0)",
                [&key],
            )
            .unwrap();
        let err = db.rekey_project("AAA", "CCC").unwrap_err().to_string();
        assert!(err.contains("AAA-1"), "{err}");
    }

    #[test]
    fn a_projects_folders_are_its_repos_and_their_worktree_folders() {
        let repo = |name: &str, root: &str, project: Option<&str>| crate::RepoConfig {
            name: name.into(),
            root: root.into(),
            integration_branch: "main".into(),
            project: project.map(str::to_string),
        };
        let root = crate::paths::unique_temp("nitely-folders", "").join("app");
        let config = crate::Config {
            repos: vec![repo("app", &root.to_string_lossy(), Some("APP")), repo("other", "C:/code/other", Some("OTH")), repo("loose", "C:/code/loose", None)],
            ..Default::default()
        };
        let mut db = db();
        db.create_project("APP", "App").unwrap();
        db.create_project("OTH", "Other").unwrap();
        let worktrees = root.parent().unwrap().join("app-worktrees");
        // Sessions in task folders of the worktree folder: one for APP, one for another project.
        for (id, task) in [("codex:a", "app-3-fix-login"), ("codex:o", "oth-4-docs"), ("codex:p", "app2-5-other"), ("codex:f", "app-2fa")] {
            let cwd = worktrees.join(task).join("src").to_string_lossy().into_owned();
            db.record(&crate::db::NewEvent { session_id: id.into(), vendor: "codex".into(), cwd, kind: "tool".into(), summary: None, state: "working".into() }).unwrap();
        }
        let folders: Vec<String> = folders_of(&config, &db, "app").iter().map(|f| crate::paths::normalize(f)).collect();
        let task = crate::paths::normalize(&worktrees.join("app-3-fix-login").to_string_lossy());
        assert_eq!(folders, [crate::paths::normalize(&root.to_string_lossy()), task], "its checkout and its own task folder, never the shared worktree folder");
        assert!(folders_of(&config, &db, "NONE").is_empty());
        // A key with digits finds its own task folders, and APP's are not APP2's.
        db.create_project("APP2", "App two").unwrap();
        let two = crate::Config { repos: vec![repo("app2", &root.to_string_lossy(), Some("APP2"))], ..Default::default() };
        let folders: Vec<String> = folders_of(&two, &db, "APP2").iter().map(|f| crate::paths::normalize(f)).collect();
        assert_eq!(folders.last(), Some(&crate::paths::normalize(&worktrees.join("app2-5-other").to_string_lossy())));
        assert_eq!(folders.len(), 2);
    }

    #[test]
    fn remove_needs_consent_for_issues() {
        let mut db = db();
        db.create_project("EMP", "Empty").unwrap();
        assert_eq!(db.remove_project("emp", false).unwrap().issues, 0);
        assert!(db.project("EMP").unwrap().is_none());

        db.create_project("FUL", "Full").unwrap();
        let key = issue(&mut db, "FUL", "Something", vec![]);
        assert!(db.remove_project("FUL", false).is_err());
        assert_eq!(db.remove_project("FUL", true).unwrap().issues, 1);
        assert!(db.issue(&key).unwrap().is_none());
        assert!(db.timeline(&key).unwrap().is_empty());
        // The key is free again.
        db.create_project("FUL", "Again").unwrap();
    }

    #[test]
    fn rename_changes_only_the_name() {
        let db = db();
        db.create_project("NAM", "Before").unwrap();
        assert_eq!(db.rename_project("nam", " After ").unwrap().name, "After");
        assert!(db.rename_project("NAM", "  ").is_err());
    }
}
