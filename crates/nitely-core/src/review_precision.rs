//! What became of each P0/P1 a review raised, per area of the code (NIT-316, part 4 of NIT-282):
//! fixed, accepted by the person as a trade-off, stale, or still open. An area whose P1s are often
//! accepted or stale is one the reviewer is too strict about, and `review_context` shows a review
//! examples from the two worst areas of what was not a P1.
//!
//! `outcomes` and `by_area` decide, over plain data; `read` gathers the reviews.

use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::db::{now, Db};
use crate::review::Finding;

/// What a raised P0/P1 came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A later round no longer has it, and it was not accepted.
    Fixed,
    /// The person accepted it as a trade-off.
    Accepted,
    /// Carried as unconfirmed (the code it cites was gone), or dropped by a fresh review before it
    /// went to the person (NIT-245).
    Stale,
    /// The latest round still carries it.
    Open,
}

/// One review round of an issue, as the classification reads it.
#[derive(Debug, Clone)]
pub struct Round {
    pub findings: Vec<Finding>,
    /// A fresh review, run without the earlier rounds before they went to the person.
    pub fresh: bool,
    pub ts: i64,
}

/// One raised P0/P1 and what it came to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Raised {
    pub issue: String,
    pub priority: u8,
    pub text: String,
    pub area: String,
    pub outcome: Outcome,
    pub ts: i64,
}

/// The words a finding was raised in, without what carrying it as unconfirmed added.
fn words(f: &Finding) -> &str {
    f.text.strip_suffix(crate::review::UNCONFIRMED).unwrap_or(&f.text)
}

/// Each P0/P1 an issue's `rounds` (oldest first) raised, and what it came to, given the findings
/// the person accepted on the issue (`accepted`, their words).
pub fn outcomes(issue: &str, rounds: &[Round], accepted: &[String]) -> Vec<Raised> {
    let mut out: Vec<Raised> = Vec::new();
    for (i, round) in rounds.iter().enumerate() {
        for f in round.findings.iter().filter(|f| f.priority <= 1 && !f.carried) {
            let text = words(f);
            if out.iter().any(|r| r.text == text) {
                continue;
            }
            let later = &rounds[i + 1..];
            let has = |r: &Round| r.findings.iter().any(|x| words(x) == text);
            let outcome = if accepted.iter().any(|a| a == text) {
                Outcome::Accepted
            } else if later.last().is_none_or(has) {
                // Still in the last round, or no round after it yet.
                Outcome::Open
            } else {
                let unconfirmed = later.iter().any(|r| r.findings.iter().any(|x| words(x) == text && (x.text != text || x.raised_as.is_some())));
                // The round that first no longer had it: a fresh one dropped it before the person saw it.
                let dropped_fresh = later.iter().find(|r| !has(r)).is_some_and(|r| r.fresh);
                if unconfirmed || dropped_fresh { Outcome::Stale } else { Outcome::Fixed }
            };
            out.push(Raised { issue: issue.to_string(), priority: f.priority, text: text.to_string(), area: area(text), outcome, ts: round.ts });
        }
    }
    out
}

/// The area a finding is about: the module of the first file it cites, `land.rs` for
/// `crates/nitely-core/src/land.rs:120`, `src/` for anything in the frontend, and `other` for a
/// finding that cites no file.
pub fn area(text: &str) -> String {
    let path_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-');
    for token in text.split(|c: char| !path_char(c)) {
        let token = token.trim_matches(|c: char| c == '.' || c == '-').replace('\\', "/");
        let Some((_, ext)) = token.rsplit_once('.') else { continue };
        if !matches!(ext, "rs" | "ts" | "tsx" | "js" | "mjs" | "cjs" | "css" | "sql" | "toml" | "yml" | "yaml" | "md" | "json") {
            continue;
        }
        if token.starts_with("src/") {
            return "src/".into();
        }
        return token.rsplit('/').next().unwrap_or(&token).to_string();
    }
    "other".into()
}

/// One area's counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Area {
    pub area: String,
    pub raised: usize,
    pub fixed: usize,
    pub accepted: usize,
    pub stale: usize,
    pub open: usize,
    /// fixed / (fixed + accepted + stale); None while none of its findings is settled.
    pub precision: Option<f64>,
}

/// The counts per area, the least precise first (then the most raised); areas with nothing settled
/// last.
pub fn by_area(raised: &[Raised]) -> Vec<Area> {
    let mut areas: BTreeMap<&str, Area> = BTreeMap::new();
    for r in raised {
        let a = areas.entry(&r.area).or_insert_with(|| Area { area: r.area.clone(), ..Default::default() });
        a.raised += 1;
        match r.outcome {
            Outcome::Fixed => a.fixed += 1,
            Outcome::Accepted => a.accepted += 1,
            Outcome::Stale => a.stale += 1,
            Outcome::Open => a.open += 1,
        }
    }
    let mut out: Vec<Area> = areas.into_values().collect();
    for a in &mut out {
        let settled = a.fixed + a.accepted + a.stale;
        a.precision = (settled > 0).then(|| a.fixed as f64 / settled as f64);
    }
    out.sort_by(|a, b| match (a.precision, b.precision) {
        (Some(x), Some(y)) => x.total_cmp(&y).then(b.raised.cmp(&a.raised)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.raised.cmp(&a.raised),
    });
    out
}

/// The least an area must have settled before it counts among the worst: one accepted P1 on its
/// own says little.
const WORST_MIN_SETTLED: usize = 3;

/// The two least precise areas with enough settled findings.
pub fn worst_areas(raised: &[Raised]) -> Vec<String> {
    by_area(raised)
        .into_iter()
        .filter(|a| a.fixed + a.accepted + a.stale >= WORST_MIN_SETTLED && a.precision.is_some_and(|p| p < 1.0))
        .take(2)
        .map(|a| a.area)
        .collect()
}

/// The accepted and stale findings of the two least precise areas (`worst_areas`), newest first:
/// examples of what was not a P1 there.
pub fn worst_examples(raised: &[Raised]) -> Vec<&Raised> {
    let worst = worst_areas(raised);
    let mut out: Vec<&Raised> = raised.iter().filter(|r| worst.contains(&r.area) && matches!(r.outcome, Outcome::Accepted | Outcome::Stale)).collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.ts));
    out
}

/// The report `nitely review-precision` prints and the Rules card shows.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub project: String,
    pub days: u32,
    pub areas: Vec<Area>,
    pub raised: Vec<Raised>,
}

/// Every P0/P1 first raised in `project`'s reviews over the last `days`, and what it came to.
pub fn raised_in(db: &Db, project: &str, days: u32) -> Result<Vec<Raised>> {
    let since = now() - i64::from(days) * 86_400;
    let mut stmt = db.conn.prepare(
        "SELECT issue_key, findings, note, ts FROM review WHERE issue_key LIKE ?1 || '-%' AND reviewer != ''
         AND issue_key IN (SELECT issue_key FROM review WHERE ts >= ?2) ORDER BY issue_key, ts, id",
    )?;
    let rows: Vec<(String, String, String, i64)> = stmt.query_map(rusqlite::params![project, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut by_issue: BTreeMap<String, Vec<Round>> = BTreeMap::new();
    for (key, findings, note, ts) in rows {
        let findings: Vec<Finding> = serde_json::from_str(&findings).unwrap_or_default();
        by_issue.entry(key.to_uppercase()).or_default().push(Round { findings, fresh: note.contains(crate::submit::FRESH), ts });
    }
    let mut out = Vec::new();
    for (key, rounds) in by_issue {
        let accepted: Vec<String> =
            db.conn.prepare("SELECT finding FROM accepted_finding WHERE issue_key = ?1 COLLATE NOCASE")?.query_map([&key], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        out.extend(outcomes(&key, &rounds, &accepted).into_iter().filter(|r| r.ts >= since));
    }
    Ok(out)
}

/// `raised_in`, counted per area.
pub fn read(db: &Db, project: &str, days: u32) -> Result<Report> {
    let raised = raised_in(db, project, days)?;
    Ok(Report { project: project.to_uppercase(), days, areas: by_area(&raised), raised })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(priority: u8, text: &str, carried: bool) -> Finding {
        Finding { priority, text: text.into(), carried, raised_as: None, now: None }
    }

    fn round(findings: Vec<Finding>, fresh: bool) -> Round {
        Round { findings, fresh, ts: 0 }
    }

    /// The four outcomes as submit records the rounds: a P1 fixed in round two, one carried as
    /// unconfirmed then gone, one a fresh review dropped, one accepted, one still carried, and a P2
    /// that counts for nothing.
    #[test]
    fn each_raised_p1_ends_fixed_accepted_stale_or_open() {
        let unconfirmed = Finding { priority: 2, text: format!("b.rs:3 never read{}", crate::review::UNCONFIRMED), carried: true, raised_as: Some(1), now: None };
        let rounds = vec![
            round(vec![f(1, "land.rs:10 leaks the lock", false), f(1, "b.rs:3 never read", false), f(0, "src/App.tsx drops the save", false), f(2, "nit", false)], false),
            round(vec![unconfirmed, f(0, "src/App.tsx drops the save", true), f(1, "crates/nitely-core/src/submit.rs:5 races", false), f(1, "accepted.rs: slow", false)], false),
            round(vec![f(1, "crates/nitely-core/src/submit.rs:5 races", true), f(1, "accepted.rs: slow", true), f(1, "fresh.rs:1 odd", false)], false),
            round(vec![f(1, "crates/nitely-core/src/submit.rs:5 races", true), f(1, "accepted.rs: slow", true)], true),
        ];
        let got: Vec<(String, Outcome, String)> = outcomes("APP-1", &rounds, &["accepted.rs: slow".into()]).into_iter().map(|r| (r.text, r.outcome, r.area)).collect();
        let want = |t: &str, o, a: &str| (t.to_string(), o, a.to_string());
        assert_eq!(
            got,
            vec![
                want("land.rs:10 leaks the lock", Outcome::Fixed, "land.rs"),
                want("b.rs:3 never read", Outcome::Stale, "b.rs"),
                want("src/App.tsx drops the save", Outcome::Fixed, "src/"),
                want("crates/nitely-core/src/submit.rs:5 races", Outcome::Open, "submit.rs"),
                want("accepted.rs: slow", Outcome::Accepted, "accepted.rs"),
                want("fresh.rs:1 odd", Outcome::Stale, "fresh.rs"),
            ]
        );
    }

    #[test]
    fn areas_rank_by_precision_and_the_worst_give_examples() {
        let r = |area: &str, outcome, ts| Raised { issue: "APP-1".into(), priority: 1, text: format!("{area} {ts}"), area: area.into(), outcome, ts };
        let raised = vec![
            r("land.rs", Outcome::Fixed, 1),
            r("land.rs", Outcome::Fixed, 2),
            r("land.rs", Outcome::Stale, 3),
            r("submit.rs", Outcome::Accepted, 4),
            r("submit.rs", Outcome::Stale, 5),
            r("submit.rs", Outcome::Fixed, 6),
            r("src/", Outcome::Accepted, 7),
            r("other", Outcome::Open, 8),
        ];
        let areas = by_area(&raised);
        let order: Vec<(&str, Option<f64>)> = areas.iter().map(|a| (a.area.as_str(), a.precision)).collect();
        assert_eq!(order, vec![("src/", Some(0.0)), ("submit.rs", Some(1.0 / 3.0)), ("land.rs", Some(2.0 / 3.0)), ("other", None)]);
        // src/ has one settled finding: too few to call it the worst.
        let examples: Vec<i64> = worst_examples(&raised).into_iter().map(|r| r.ts).collect();
        assert_eq!(examples, vec![5, 4, 3]);
    }

    #[test]
    fn a_finding_is_about_the_first_file_it_cites() {
        assert_eq!(area("[P1] crates/nitely-core/src/land.rs:120: the lock leaks"), "land.rs");
        assert_eq!(area("src\\ui\\Card.tsx renders twice"), "src/");
        assert_eq!(area("In `src-tauri/src/lib.rs`, the command"), "lib.rs");
        assert_eq!(area("The change does not do what the issue asks."), "other");
        assert_eq!(area("Version 1.2 is wrong"), "other");
    }
}
