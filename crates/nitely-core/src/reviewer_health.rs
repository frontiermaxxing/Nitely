//! Whether each configured reviewer can run (NIT-362). When the playbook's reviewer cannot run (not
//! signed in, over quota, not installed), a review falls back to another, possibly the author's own
//! vendor. The person hears of it once, on the updates feed and the Overview, not once per review,
//! until that reviewer reviews again; the reviews done meanwhile by the author's own vendor are
//! listed so they can be rerun.

use anyhow::Result;
use serde::Serialize;

use crate::db::{now, Db};
use crate::issues::Actor;

/// The event a reviewer that stopped being able to run leaves, once.
pub const UNAVAILABLE: &str = "reviewer_unavailable";
/// The event it leaves when it reviews again.
pub const AVAILABLE: &str = "reviewer_available";
/// Who records them: Nitely, not the agent whose submit noticed, so every reader hears it.
const ACTOR: &str = "nitely-review";

/// A reviewer that cannot run, since when, why, and what the person can do about it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Down {
    pub reviewer: String,
    pub why: String,
    pub hint: String,
    pub since: i64,
}

/// An open change whose latest review was by the author's own vendor.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SameVendor {
    pub key: String,
    pub title: String,
    pub state: String,
    pub reviewer: String,
    pub note: String,
    pub ts: i64,
}

/// "Codex" for `codex`: how the person knows the tool.
pub fn display(reviewer: &str) -> String {
    let mut c = reviewer.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// "Codex can't review: not signed in (run `codex login`)".
pub fn summary(reviewer: &str, why: &str, hint: &str) -> String {
    let hint = if hint.is_empty() { String::new() } else { format!(" ({hint})") };
    format!("{} can't review: {why}{hint}", display(reviewer))
}

/// The note `submit` leaves on a review the author's own vendor did.
pub const SAME_VENDOR_NOTE: &str = "same vendor";

impl Db {
    /// `reviewer` could not run for the review of `issue_key`. The first time since it last ran,
    /// it is recorded and told; later misses change nothing.
    pub fn reviewer_could_not_run(&self, issue_key: &str, reviewer: &str, why: &str, hint: &str) -> Result<()> {
        let fresh = self.conn.execute(
            "INSERT OR IGNORE INTO reviewer_down (reviewer, why, hint, since) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![reviewer, why, hint, now()],
        )? == 1;
        if fresh {
            self.log_event(&Actor::Agent(ACTOR.into()), Some(issue_key), None, UNAVAILABLE, &summary(reviewer, why, hint))?;
        }
        Ok(())
    }

    /// `reviewer` ran a review: if it was down, it is back, and the person is told.
    pub fn reviewer_ran(&self, reviewer: &str) -> Result<()> {
        if self.conn.execute("DELETE FROM reviewer_down WHERE reviewer = ?1", [reviewer])? == 1 {
            self.log_event(&Actor::Agent(ACTOR.into()), None, None, AVAILABLE, &format!("{} reviews again", display(reviewer)))?;
        }
        Ok(())
    }

    /// The reviewers that cannot run, the longest down first.
    pub fn reviewers_down(&self) -> Result<Vec<Down>> {
        let mut stmt = self.conn.prepare("SELECT reviewer, why, hint, since FROM reviewer_down ORDER BY since, reviewer")?;
        let rows = stmt.query_map([], |r| Ok(Down { reviewer: r.get(0)?, why: r.get(1)?, hint: r.get(2)?, since: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Open changes whose latest review was by their author's own vendor, newest first: what a
    /// rerun would put in front of another vendor.
    pub fn same_vendor_reviews(&self) -> Result<Vec<SameVendor>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.issue_key, i.title, i.state, r.reviewer, r.note, r.ts FROM review r JOIN active_issue i ON i.key = r.issue_key
             WHERE r.id = (SELECT MAX(x.id) FROM review x WHERE x.issue_key = r.issue_key AND x.reviewer != '')
               AND r.note LIKE ?1 AND i.state_type NOT IN ('completed', 'canceled')
             ORDER BY r.ts DESC, r.id DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([format!("{SAME_VENDOR_NOTE}%")], |r| {
            Ok(SameVendor { key: r.get(0)?, title: r.get(1)?, state: r.get(2)?, reviewer: r.get(3)?, note: r.get::<_, Option<String>>(4)?.unwrap_or_default(), ts: r.get(5)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sends `keys` back for a review by another vendor: each that is still listed by
    /// `same_vendor_reviews` and waiting (In Review or Needs You) goes back to In Progress with a
    /// note for its agent to submit again. Returns the keys it sent back.
    pub fn rerun_same_vendor_reviews(&mut self, keys: &[String], actor: &Actor) -> Result<Vec<String>> {
        let listed = self.same_vendor_reviews()?;
        let mut sent = Vec::new();
        for key in keys {
            let Some(row) = listed.iter().find(|r| r.key.eq_ignore_ascii_case(key)) else { continue };
            if row.state != "In Review" && row.state != "Needs You" {
                continue;
            }
            self.move_issue(&row.key, "In Progress", None, None, actor)?;
            self.add_note(
                &row.key,
                &format!("Its last review was by {}, the author's own vendor, because the usual reviewer couldn't run. Submit again for a review by another vendor.", display(&row.reviewer)),
                "note",
                actor,
            )?;
            sent.push(row.key.clone());
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;

    fn issue(db: &mut Db, title: &str) -> String {
        let new = NewIssue { project: "NIT".into(), title: title.into(), kind: "bug".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() };
        db.create_issue(&new, &Actor::Human).unwrap().key
    }

    fn review(db: &Db, key: &str, reviewer: &str, note: &str) {
        db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, flagged, ts, note) VALUES (?1, '', 'c', ?2, 'pass', 'agent', '[]', '[]', '[]', ?3, ?4)",
                rusqlite::params![key, reviewer, now(), note],
            )
            .unwrap();
    }

    /// NIT-362: a reviewer that can't run is told once however many reviews miss it, clears when
    /// it reviews again, and the reviews its absence left to the author's own vendor can be sent
    /// back for another vendor's.
    #[test]
    fn a_reviewer_that_cannot_run_is_told_once_and_its_same_vendor_reviews_can_be_rerun() {
        let dir = crate::paths::unique_temp("nitely-reviewer-down", "");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let events = |db: &Db, kind: &str| -> Vec<String> {
            let mut s = db.conn.prepare("SELECT summary FROM event WHERE type = ?1 ORDER BY id").unwrap();
            s.query_map([kind], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
        };
        let a = issue(&mut db, "First change");
        let b = issue(&mut db, "Second change");

        db.reviewer_could_not_run(&a, "codex", "not signed in", "run `codex login`").unwrap();
        db.reviewer_could_not_run(&b, "codex", "not signed in", "run `codex login`").unwrap();
        assert_eq!(events(&db, UNAVAILABLE), ["Codex can't review: not signed in (run `codex login`)"], "once, not per review");
        assert_eq!(db.reviewers_down().unwrap().iter().map(|d| d.reviewer.as_str()).collect::<Vec<_>>(), ["codex"]);

        // Meanwhile both were reviewed by the author's own vendor; one has since landed.
        review(&db, &a, "claude", "same vendor: codex could not run: not signed in");
        review(&db, &b, "claude", "same vendor: codex could not run: not signed in");
        db.move_issue(&a, "In Review", None, None, &Actor::Human).unwrap();
        db.move_issue(&b, "Done", Some("landed"), None, &Actor::Human).unwrap();
        let listed: Vec<String> = db.same_vendor_reviews().unwrap().into_iter().map(|r| r.key).collect();
        assert_eq!(listed, [a.clone()], "only open changes");

        db.reviewer_ran("codex").unwrap();
        db.reviewer_ran("codex").unwrap();
        assert!(db.reviewers_down().unwrap().is_empty());
        assert_eq!(events(&db, AVAILABLE), ["Codex reviews again"], "told once that it is back");

        assert_eq!(db.rerun_same_vendor_reviews(&[a.clone(), b.clone()], &Actor::Human).unwrap(), [a.clone()]);
        assert_eq!(db.issue(&a).unwrap().unwrap().state, "In Progress", "back to its agent to submit again");

        // A later miss is news again.
        db.reviewer_could_not_run(&a, "codex", "over quota", "").unwrap();
        assert_eq!(events(&db, UNAVAILABLE).last().map(String::as_str), Some("Codex can't review: over quota"));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
