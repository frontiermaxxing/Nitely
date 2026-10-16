//! A project's brief and setup marker. The brief is one Markdown document per project: what agents
//! learned about it as a whole (the vision as read from the code, the state of each area, a target
//! order, the questions only the owner can answer), so a new agent reads it instead of sweeping the
//! repo again. The setup marker says the first-run sweep happened, at which commit, so an agent can
//! tell a never-set-up project from one with no open issues, and a refresh can sweep only what
//! changed since.

use crate::db::{now, Db};
use crate::issues::Actor;
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

/// Large enough for a detailed brief, small enough that every agent can read it whole.
pub const MAX_BRIEF: usize = 100_000;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectBrief {
    pub project: String,
    pub name: String,
    pub open_issues: i64,
    pub brief: String,
    /// Bumped on every write. A writer passes the revision it read, so nobody overwrites an edit
    /// they have not seen.
    pub revision: i64,
    pub updated_at: Option<i64>,
    pub updated_by: Option<String>,
    /// None until the setup sweep has run.
    pub setup: Option<Setup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Setup {
    pub sha: String,
    pub at: i64,
    pub by: String,
}

impl Db {
    pub fn project_brief(&self, key: &str) -> Result<ProjectBrief> {
        let p = self.project(key)?.with_context(|| format!("no project {key}"))?;
        let row = self
            .conn
            .query_row(
                "SELECT brief, revision, updated_at, updated_by, setup_sha, setup_at, setup_by FROM project_brief WHERE project = ?1",
                [&p.key],
                |r| {
                    let setup = match (r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, Option<String>>(6)?) {
                        (Some(sha), Some(at), Some(by)) => Some(Setup { sha, at, by }),
                        _ => None,
                    };
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get(2)?, r.get(3)?, setup))
                },
            )
            .optional()?;
        let (brief, revision, updated_at, updated_by, setup) = row.unwrap_or_default();
        Ok(ProjectBrief { project: p.key, name: p.name, open_issues: p.open_issues, brief, revision, updated_at, updated_by, setup })
    }

    /// Replaces the brief. `revision` is the one the writer read (0 for a project that never had
    /// one); when someone wrote since, nothing changes and the error says who.
    pub fn set_project_brief(&self, key: &str, brief: &str, revision: i64, actor: &Actor) -> Result<ProjectBrief> {
        let current = self.project_brief(key)?;
        if brief.len() > MAX_BRIEF {
            bail!("the brief is {} bytes; keep it under {MAX_BRIEF} (link to docs in the repo for detail)", brief.len());
        }
        if revision != current.revision {
            bail!(
                "the brief changed since you read it (you read revision {revision}; it is now {} by {}). Read it again with get_project and merge your changes into it",
                current.revision,
                current.updated_by.as_deref().unwrap_or("someone")
            );
        }
        // The revision check is repeated in the write, so two writers racing past the read above
        // cannot both succeed.
        let written = self.conn.execute(
            "INSERT INTO project_brief (project, brief, revision, updated_at, updated_by) VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT (project) DO UPDATE SET brief = excluded.brief, revision = revision + 1,
               updated_at = excluded.updated_at, updated_by = excluded.updated_by
             WHERE project_brief.revision = ?5",
            params![current.project, brief, now(), actor.id(), revision],
        )?;
        if written == 0 {
            bail!("the brief changed since you read it; read it again with get_project and merge your changes into it");
        }
        self.project_brief(&current.project)
    }

    /// Records that the setup sweep read the project at `sha`. Running it again (a refresh) moves
    /// the marker to the new commit.
    pub fn mark_setup_complete(&self, key: &str, sha: &str, actor: &Actor) -> Result<ProjectBrief> {
        let current = self.project_brief(key)?;
        let sha = sha.trim().to_ascii_lowercase();
        if !(7..=64).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("give the commit the sweep read as a hex SHA, e.g. from `git rev-parse HEAD`");
        }
        self.conn.execute(
            "INSERT INTO project_brief (project, setup_sha, setup_at, setup_by) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project) DO UPDATE SET setup_sha = excluded.setup_sha, setup_at = excluded.setup_at,
               setup_by = excluded.setup_by",
            params![current.project, sha, now(), actor.id()],
        )?;
        self.project_brief(&current.project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let dir = std::env::temp_dir().join(format!(
            "nitely-brief-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        db
    }

    #[test]
    fn a_new_project_has_an_empty_brief_and_no_setup() {
        let db = db();
        let b = db.project_brief("nit").unwrap();
        assert_eq!((b.project.as_str(), b.brief.as_str(), b.revision), ("NIT", "", 0));
        assert!(b.setup.is_none() && b.updated_by.is_none());
        assert!(db.project_brief("NOPE").is_err());
    }

    #[test]
    fn writes_need_the_revision_they_read() {
        let db = db();
        let agent = Actor::Agent("claude:/w".into());
        let b = db.set_project_brief("NIT", "# Nitely\n", 0, &agent).unwrap();
        assert_eq!((b.revision, b.updated_by.as_deref()), (1, Some("claude:/w")));
        let b = db.set_project_brief("NIT", "# Nitely, edited\n", 1, &Actor::Human).unwrap();
        assert_eq!((b.revision, b.updated_by.as_deref()), (2, Some("human")));
        // An agent that read revision 1 cannot overwrite the human's edit.
        let err = db.set_project_brief("NIT", "# stale\n", 1, &agent).unwrap_err().to_string();
        assert!(err.contains("changed since you read it") && err.contains("by human"), "{err}");
        assert_eq!(db.project_brief("NIT").unwrap().brief, "# Nitely, edited\n");
        assert!(db.set_project_brief("NIT", &"x".repeat(MAX_BRIEF + 1), 2, &agent).is_err());
    }

    #[test]
    fn setup_marker_keeps_the_brief_and_moves_on_refresh() {
        let db = db();
        let agent = Actor::Agent("codex:/w".into());
        db.set_project_brief("NIT", "brief", 0, &agent).unwrap();
        assert!(db.mark_setup_complete("NIT", "not a sha", &agent).is_err());
        let b = db.mark_setup_complete("NIT", "ABCDEF1234567", &agent).unwrap();
        let setup = b.setup.unwrap();
        assert_eq!((setup.sha.as_str(), setup.by.as_str()), ("abcdef1234567", "codex:/w"));
        assert_eq!((b.brief.as_str(), b.revision), ("brief", 1));
        let b = db.mark_setup_complete("NIT", "1234567", &agent).unwrap();
        assert_eq!(b.setup.unwrap().sha, "1234567");
    }

    #[test]
    fn the_brief_follows_a_new_key_and_goes_with_the_project() {
        let mut db = db();
        db.set_project_brief("NIT", "brief", 0, &Actor::Human).unwrap();
        db.mark_setup_complete("NIT", "abc1234", &Actor::Human).unwrap();
        db.rekey_project("NIT", "NX").unwrap();
        let b = db.project_brief("NX").unwrap();
        assert_eq!((b.brief.as_str(), b.setup.map(|s| s.sha)), ("brief", Some("abc1234".to_string())));
        assert_eq!(db.project_brief("nit").unwrap().project, "NX", "the old key still reaches it");
        db.remove_project("NX", false).unwrap();
        db.create_project("NX", "Again").unwrap();
        let b = db.project_brief("NX").unwrap();
        assert!(b.brief.is_empty() && b.setup.is_none(), "a new project with the same key starts empty");
    }

    #[test]
    fn setup_can_come_before_any_brief() {
        let db = db();
        let b = db.mark_setup_complete("NIT", "abc1234", &Actor::Human).unwrap();
        assert!(b.setup.is_some());
        assert_eq!((b.brief.as_str(), b.revision), ("", 0));
        assert_eq!(db.set_project_brief("NIT", "first", 0, &Actor::Human).unwrap().revision, 1);
    }
}
