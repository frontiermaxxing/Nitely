//! Changing projects after they exist: a new name, a new key (the issue prefix), or removing one.
//!
//! A new key re-keys every issue of the project (NIT-12 becomes NX-12) and every row that names
//! one, in one transaction. The old key stays behind as an alias, so a `nitely://issue/NIT-12`
//! link, a lookup by the old key and an old branch (`claude/nit-12-...`) still find the issue.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;

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
    ("ask", "issue_key"),
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
    #[serde(default)]
    pub repos: usize,
    #[serde(default)]
    pub machines: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    pub key: String,
    /// Issues deleted with it (only when asked to).
    pub issues: i64,
    /// Repos and machines that named it and now name no project.
    #[serde(default)]
    pub repos: usize,
    #[serde(default)]
    pub machines: usize,
}

/// Re-keys a project in the database, then in the repos and machine tokens that name it.
pub fn rekey(db: &mut Db, from: &str, to: &str) -> Result<Rekeyed> {
    let mut done = db.rekey_project(from, to)?;
    let mut config = crate::Config::load()?;
    done.repos = config.retarget_project(&done.from, Some(&done.to));
    if done.repos > 0 {
        config.save()?;
    }
    done.machines = crate::relay::retarget_project(&done.from, Some(&done.to))?;
    Ok(done)
}

/// Removes a project from the database; repos and machine tokens that named it name none.
pub fn remove(db: &mut Db, key: &str, with_issues: bool) -> Result<Removed> {
    let mut done = db.remove_project(key, with_issues)?;
    let mut config = crate::Config::load()?;
    done.repos = config.retarget_project(&done.key, None);
    if done.repos > 0 {
        config.save()?;
    }
    done.machines = crate::relay::retarget_project(&done.key, None)?;
    Ok(done)
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

    pub fn rename_project(&self, key: &str, name: &str) -> Result<crate::issues::Project> {
        let project = self.project(key)?.with_context(|| format!("no project {key}"))?;
        let name = name.trim();
        if name.is_empty() {
            bail!("a project needs a name");
        }
        self.conn.execute("UPDATE project SET name = ?2 WHERE key = ?1", params![project.key, name])?;
        self.project(&project.key)?.context("project vanished after rename")
    }

    /// Gives a project a new key, and every one of its issues the new prefix.
    ///
    /// Refused while any of its issues is checked out in an open task (that worktree's branch
    /// names the old key), and when issues with the prefix are mirrored from another tracker
    /// (their keys are the tracker's, not Nitely's).
    pub fn rekey_project(&mut self, from: &str, to: &str) -> Result<Rekeyed> {
        let project = self.project(from)?.with_context(|| format!("no project {from}"))?;
        let from = project.key;
        let to = valid_key(to)?;
        if to == from {
            bail!("{from} already has that key");
        }
        let (taken, alias_of): (bool, Option<String>) = (
            self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM project WHERE key = ?1)
                     OR EXISTS (SELECT 1 FROM issue WHERE key LIKE ?1 || '-%')",
                [&to],
                |r| r.get(0),
            )?,
            self.conn
                .query_row("SELECT project FROM project_alias WHERE old_key = ?1", [&to], |r| r.get(0))
                .optional()?,
        );
        if taken {
            bail!("{to} is already a project or an issue prefix");
        }
        // An old key can come back to the project that had it, but not go to another one, or its
        // old links would open the wrong issues.
        if alias_of.as_deref().is_some_and(|p| p != from) {
            bail!("{to} was an earlier key of {}, and its old links still lead there", alias_of.unwrap());
        }
        let mirrored: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM issue WHERE key LIKE ?1 || '-%' AND origin <> 'nitely'",
            [&from],
            |r| r.get(0),
        )?;
        if mirrored > 0 {
            bail!("{mirrored} issue(s) with the prefix {from} are mirrored from another tracker, which owns their keys");
        }
        let open: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT issue_key FROM task WHERE closed_at IS NULL AND issue_key LIKE ?1 || '-%' ORDER BY issue_key",
            )?;
            let rows = stmt.query_map([&from], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if !open.is_empty() {
            bail!(
                "{} {} checked out in a worktree whose branch names {from}; land or close {} first",
                open.join(", "),
                if open.len() == 1 { "is" } else { "are" },
                if open.len() == 1 { "it" } else { "them" }
            );
        }

        let tx = self.conn.transaction()?;
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
        tx.execute("DELETE FROM project_alias WHERE old_key = ?1", [&to])?;
        tx.execute(
            "INSERT OR REPLACE INTO project_alias (old_key, project, created_at) VALUES (?1, ?2, ?3)",
            params![from, to, now()],
        )?;
        tx.commit()?;
        Ok(Rekeyed { from, to, issues, repos: 0, machines: 0 })
    }

    /// Removes a project. Refused while it has issues, unless `with_issues`, which deletes them
    /// and everything recorded about them; refused either way while one is checked out.
    pub fn remove_project(&mut self, key: &str, with_issues: bool) -> Result<Removed> {
        let project = self.project(key)?.with_context(|| format!("no project {key}"))?;
        let key = project.key;
        let issues: i64 = self.conn.query_row("SELECT COUNT(*) FROM issue WHERE project = ?1", [&key], |r| r.get(0))?;
        if issues > 0 && !with_issues {
            bail!("{key} has {issues} issue(s); remove it with its issues, or move them first");
        }
        let open: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM task WHERE closed_at IS NULL AND issue_key IN (SELECT key FROM issue WHERE project = ?1)",
            [&key],
            |r| r.get(0),
        )?;
        if open > 0 {
            bail!("{open} of {key}'s issues are checked out in a worktree; land or close them first");
        }
        let tx = self.conn.transaction()?;
        let of_project = "(SELECT key FROM issue WHERE project = ?1)";
        for (table, column) in KEY_COLUMNS.iter().filter(|(t, _)| *t != "issue") {
            tx.execute(&format!("DELETE FROM {table} WHERE {column} IN {of_project}"), [&key])?;
        }
        tx.execute("DELETE FROM issue WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM project_alias WHERE project = ?1", [&key])?;
        tx.execute("DELETE FROM project WHERE key = ?1", [&key])?;
        tx.commit()?;
        Ok(Removed { key, issues, repos: 0, machines: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::{Actor, NewIssue};

    fn db() -> Db {
        // A folder per test, so tests running at once never share a database.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nitely-projects-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
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
        assert!(db.project("OLD").unwrap().is_none());
        let moved = db.issue("NEW-2").unwrap().unwrap();
        assert_eq!(moved.project.as_deref(), Some("NEW"));
        assert_eq!(moved.blocked_by, vec!["NEW-1".to_string()]);
        assert!(!db.timeline("NEW-1").unwrap().is_empty(), "the timeline follows the issue");
        // Old keys and links still find it.
        assert_eq!(db.issue("old-2").unwrap().unwrap().key, "NEW-2");
        assert_eq!(db.alias_of("OLD-7").unwrap().as_deref(), Some("NEW-7"));
        // New issues continue the numbering under the new key.
        assert_eq!(issue(&mut db, "NEW", "Third", vec![]), "NEW-3");
        // The old key cannot be given to another project, but can come back to this one.
        db.create_project("OTH", "Other").unwrap();
        assert!(db.rekey_project("OTH", "OLD").is_err());
        db.rekey_project("NEW", "OLD").unwrap();
        assert_eq!(db.issue("NEW-1").unwrap().unwrap().key, "OLD-1");
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
