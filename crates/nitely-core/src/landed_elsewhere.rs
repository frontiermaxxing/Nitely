//! Done for an issue whose change reached the integration branch without landing by itself
//! (NIT-361): merged by hand, or folded into another issue's change that landed. Done is refused
//! while a submitted change has not landed since (`Db::check_move`); this records the landing the
//! integration branch shows, so Done goes through and the goal counts the work as shipped.

use anyhow::Result;
use std::path::Path;

use crate::db::Db;
use crate::issues::Actor;
use crate::playbook::{self, Playbook};

/// Why a handed-in change is on the integration branch `base` although it never landed by itself,
/// from what was read: `on_base`, a head it was submitted at that `base` contains, and `landed_with`,
/// an issue the evidence names that landed after this one's last submit. None: it is not.
pub fn reached(on_base: Option<&str>, landed_with: Option<&str>, base: &str) -> Option<String> {
    if let Some(head) = on_base {
        return Some(format!("its submitted change {} is on {base}", &head[..head.len().min(9)]));
    }
    landed_with.map(|key| format!("it landed on {base} in {key}'s change"))
}

/// The issue keys `evidence` names, as written (`NIT-326`), other than `own`.
pub fn named_keys<'a>(evidence: &'a str, own: &str) -> Vec<&'a str> {
    evidence
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.split_once('-').is_some_and(|(p, n)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_uppercase()) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
        .filter(|w| !w.eq_ignore_ascii_case(own))
        .collect()
}

/// Before Done: when `key`'s last submitted change has not landed by itself, but the integration
/// branch of the repo at `root` contains it, or `evidence` names an issue that landed after it,
/// records the landing as a `landed` event and says what it recorded. None: nothing to record.
pub fn record(db: &Db, root: &Path, key: &str, evidence: &str, actor: &Actor) -> Result<Option<String>> {
    let Some(issue) = db.issue(key)? else { return Ok(None) };
    let (submitted, landed): (Option<i64>, Option<i64>) = db.conn.query_row(
        "SELECT (SELECT MAX(id) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'submit'),
                (SELECT MAX(id) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'landed')",
        [&issue.key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let Some(submitted) = submitted.filter(|s| landed.is_none_or(|l| l < *s)) else { return Ok(None) };
    let base = Playbook::load(root)?.integration_branch().to_string();
    let heads: Vec<String> = {
        let mut stmt = db.conn.prepare("SELECT head FROM submit_job WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC")?;
        let rows = stmt.query_map([&issue.key], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        rows
    };
    let on_base = heads.iter().find(|h| playbook::git(root, &["merge-base", "--is-ancestor", h.as_str(), &base]).is_ok());
    let landed_with = named_keys(evidence, &issue.key).into_iter().find(|other| {
        db.conn
            .query_row("SELECT EXISTS(SELECT 1 FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'landed' AND id > ?2)", rusqlite::params![other, submitted], |r| r.get::<_, bool>(0))
            .unwrap_or(false)
    });
    let Some(why) = reached(on_base.map(String::as_str), landed_with, &base) else { return Ok(None) };
    let said = format!("Landed without a land of its own: {why}");
    db.log_event(actor, Some(&issue.key), None, "landed", &said)?;
    Ok(Some(said))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_on_main_or_landed_with_another_issue_counts_as_landed() {
        assert_eq!(reached(Some("a466ce6d1f00"), None, "main").as_deref(), Some("its submitted change a466ce6d1 is on main"));
        assert_eq!(reached(None, Some("APP-326"), "dev").as_deref(), Some("it landed on dev in APP-326's change"));
        assert_eq!(reached(Some("a466ce6d"), Some("APP-326"), "main").as_deref(), Some("its submitted change a466ce6d is on main"), "the branch says it first");
        assert_eq!(reached(None, None, "main"), None, "nothing shows it landed: Done stays refused");
        assert_eq!(named_keys("Landed in APP-326 (a466ce6d), with app-433's commit.", "APP-433"), ["APP-326"]);
        assert!(named_keys("ran the tests: 12 passed, utf-8 fine", "APP-1").is_empty());
    }
}
