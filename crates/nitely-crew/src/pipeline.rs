//! How Nitely's own pipeline performs, per project and week (NIT-703): reviews and their rounds,
//! the findings the person accepted or split off, lane waits against run time, why lands stopped,
//! questions to the person, each issue's time from claim to landed, and the tokens it took. The
//! numbers a process audit computed by hand, read the same way before and after each change.
//!
//! A week is Monday 00:00 UTC to the next. An issue counts in the week it landed, with everything
//! of its own (reviews, findings, submits, tokens) wherever those fell; lane runs, land stops and
//! questions count in the week they happened.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use nitely_core::config::Config;
use nitely_core::db::Db;
use serde::Serialize;

const WEEK: i64 = 7 * 86_400;

/// Said with every report: the issues ran side by side, so their times overlap.
pub const NOTE: &str = "Issues run in parallel, so their times overlap: a lane's waits and runs, and each issue's claim-to-landed spans, are not one queue and do not add up to the week. Medians are per run or per issue.";

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub project: String,
    /// Oldest first.
    pub weeks: Vec<Week>,
    pub note: &'static str,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Week {
    /// Its Monday, YYYY-MM-DD (UTC).
    pub week: String,
    /// Issues landed this week.
    pub landed: usize,
    pub reviews: Reviews,
    pub findings: Findings,
    /// Per lane kind (gate, land, run).
    pub lanes: Vec<Lane>,
    /// Lands that stopped, by cause: conflict, changed_since_review, review, gate, other.
    pub land_stops: BTreeMap<String, usize>,
    pub asks: Asks,
    pub issues: IssueTimes,
    /// None when the agents' session files could not be read.
    pub tokens: Option<Tokens>,
}

/// The reviews of the issues landed this week.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Reviews {
    pub reviews: usize,
    pub per_landed: Option<f64>,
    /// The share that asked for changes.
    pub changes_rate: Option<f64>,
    /// Round 1 is each issue's first review.
    pub by_round: Vec<Round>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Round {
    pub round: usize,
    pub reviews: usize,
    /// P0/P1 findings no earlier round of the same issue raised.
    pub new_blocking: usize,
    pub per_review: f64,
}

/// The blocking findings of the issues landed this week, and how many the person accepted as they
/// are or split to a follow-up (an acceptance whose reason names another issue).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Findings {
    pub blocking: usize,
    pub accepted: usize,
    pub split: usize,
    /// (accepted + split) / blocking.
    pub share: Option<f64>,
}

/// A lane kind's runs queued this week: how long they waited for a lane against how long they ran.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Lane {
    pub kind: String,
    pub runs: usize,
    pub wait_median_secs: i64,
    pub run_median_secs: i64,
    pub wait_secs: i64,
    pub run_secs: i64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Asks {
    pub asks: usize,
    /// Questions about a review finding (accept it or not).
    pub about_findings: usize,
    pub per_landed: Option<f64>,
}

/// Medians over the issues landed this week, in seconds.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct IssueTimes {
    pub claim_to_first_submit: Option<i64>,
    pub submit_rounds: Option<f64>,
    /// The time between a submit's answer and the next submit, summed per issue.
    pub fixing_between_rounds: Option<i64>,
    pub last_submit_to_landed: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Tokens {
    /// New input, cache writes and output (`Usage::tokens`), over the issues landed this week.
    pub per_landed: Option<u64>,
    /// What one model call read: new input, cache reads and cache writes, median over the week's
    /// calls in this project.
    pub median_context: Option<u64>,
    pub calls: usize,
}

// ----- facts -----

/// A review round as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRow {
    pub issue: String,
    pub verdict: String,
    pub ts: i64,
    /// (priority, text, carried from an earlier round)
    pub findings: Vec<(i64, String, bool)>,
    /// A pass carried over from the round before, for the same diff under the same rules: no
    /// reviewer ran, so it is no review (NIT-714).
    pub reused: bool,
}

/// One submit of an issue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Submit {
    pub queued: i64,
    pub ended: Option<i64>,
}

/// What one landed issue went through.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Landed {
    pub issue: String,
    pub landed: i64,
    pub claimed: Option<i64>,
    /// Oldest first.
    pub submits: Vec<Submit>,
}

/// An acceptance of a finding.
#[derive(Debug, Clone, PartialEq)]
pub struct Acceptance {
    pub issue: String,
    pub priority: i64,
    pub reason: String,
}

/// Verdicts that are a reviewer's reading of the change (not batched, disabled or skipped).
const REVIEWED: &[&str] = &["pass", "changes"];

/// `reviews` (any order) as rounds per issue, for the issues in `landed`.
pub fn reviews(landed: &[&str], rows: &[ReviewRow]) -> (Reviews, usize) {
    let landed: HashSet<&str> = landed.iter().copied().collect();
    let mut by_issue: BTreeMap<&str, Vec<&ReviewRow>> = BTreeMap::new();
    for r in rows.iter().filter(|r| landed.contains(r.issue.as_str()) && REVIEWED.contains(&r.verdict.as_str()) && !r.reused) {
        by_issue.entry(&r.issue).or_default().push(r);
    }
    let mut rounds: Vec<Round> = Vec::new();
    let (mut total, mut changes, mut blocking) = (0, 0, 0);
    for rows in by_issue.values_mut() {
        rows.sort_by_key(|r| r.ts);
        let mut seen: HashSet<&str> = HashSet::new();
        for (i, r) in rows.iter().enumerate() {
            if rounds.len() <= i {
                rounds.push(Round { round: i + 1, ..Default::default() });
            }
            let new = r.findings.iter().filter(|(p, text, carried)| *p <= 1 && !carried && seen.insert(text.as_str())).count();
            rounds[i].reviews += 1;
            rounds[i].new_blocking += new;
            total += 1;
            blocking += new;
            changes += usize::from(r.verdict == "changes");
        }
    }
    for r in &mut rounds {
        r.per_review = r.new_blocking as f64 / r.reviews as f64;
    }
    let reviews = Reviews { reviews: total, per_landed: ratio(total, landed.len()), changes_rate: ratio(changes, total), by_round: rounds };
    (reviews, blocking)
}

/// The blocking findings and what the person did with them: accepted as they are, or split off (the
/// reason names another issue, which takes it).
pub fn findings(blocking: usize, landed: &[&str], accepted: &[Acceptance]) -> Findings {
    let key = regex::Regex::new(r"\b[A-Z][A-Z0-9]*-\d+\b").expect("the issue key pattern");
    let (mut acc, mut split) = (0, 0);
    for a in accepted.iter().filter(|a| a.priority <= 1 && landed.iter().any(|k| k.eq_ignore_ascii_case(&a.issue))) {
        if key.find_iter(&a.reason).any(|m| !m.as_str().eq_ignore_ascii_case(&a.issue)) {
            split += 1;
        } else {
            acc += 1;
        }
    }
    Findings { blocking, accepted: acc, split, share: ratio(acc + split, blocking) }
}

/// Why a land stopped, from what it said (`land_failed`).
pub fn stop_cause(summary: &str) -> &'static str {
    if summary.contains("changed since its review") {
        "changed_since_review"
    } else if summary.contains("conflict") {
        "conflict"
    } else if summary.contains("review asked for changes") || summary.contains("has no review") {
        "review"
    } else if summary.contains("gate") || summary.contains("checks failed") {
        "gate"
    } else {
        "other"
    }
}

/// Lane runs (kind, queued, started, ended) per kind.
pub fn lanes(runs: &[(String, i64, i64, i64)]) -> Vec<Lane> {
    let mut by: BTreeMap<&str, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for (kind, queued, started, ended) in runs {
        let e = by.entry(kind).or_default();
        e.0.push((started - queued).max(0));
        e.1.push((ended - started).max(0));
    }
    by.into_iter()
        .map(|(kind, (mut wait, mut run))| Lane {
            kind: kind.into(),
            runs: wait.len(),
            wait_secs: wait.iter().sum(),
            run_secs: run.iter().sum(),
            wait_median_secs: median(&mut wait).unwrap_or(0),
            run_median_secs: median(&mut run).unwrap_or(0),
        })
        .collect()
}

/// Claim to first submit, submit rounds, fixing between rounds, and last submit to landed: medians
/// over `landed`.
pub fn issue_times(landed: &[Landed]) -> IssueTimes {
    let (mut claim, mut rounds, mut fixing, mut last) = (vec![], vec![], vec![], vec![]);
    for l in landed.iter().filter(|l| !l.submits.is_empty()) {
        let first = l.submits[0].queued;
        if let Some(c) = l.claimed.filter(|c| *c <= first) {
            claim.push(first - c);
        }
        rounds.push(l.submits.len() as i64);
        fixing.push(l.submits.windows(2).filter_map(|w| w[0].ended.map(|e| (w[1].queued - e).max(0))).sum());
        last.push((l.landed - l.submits[l.submits.len() - 1].queued).max(0));
    }
    let n = rounds.len();
    IssueTimes {
        claim_to_first_submit: median(&mut claim),
        submit_rounds: (n > 0).then(|| {
            rounds.sort_unstable();
            if n % 2 == 1 { rounds[n / 2] as f64 } else { (rounds[n / 2 - 1] + rounds[n / 2]) as f64 / 2.0 }
        }),
        fixing_between_rounds: median(&mut fixing),
        last_submit_to_landed: median(&mut last),
    }
}

fn median(v: &mut [i64]) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2 })
}

fn ratio(a: usize, b: usize) -> Option<f64> {
    (b > 0).then(|| (a as f64 / b as f64 * 100.0).round() / 100.0)
}

/// The Monday 00:00 UTC that starts the week holding `ts`.
pub fn week_start(ts: i64) -> i64 {
    let day = ts.div_euclid(86_400);
    // 1970-01-01 was a Thursday: three days after a Monday.
    (day - (day + 3).rem_euclid(7)) * 86_400
}

// ----- reading -----

/// The last `weeks` weeks of `project` (this one included), oldest first. `tokens` reads the
/// agents' session files for the token figures; without it they are left out.
pub fn read(db: &Db, config: Option<&Config>, project: &str, weeks: usize, now: i64) -> Result<Report> {
    let weeks = weeks.clamp(1, 52) as i64;
    let first = week_start(now) - (weeks - 1) * WEEK;
    let usage = config.map(|c| crate::usage::scan_projects(c, db, first - 4 * WEEK));
    let mut out = Vec::new();
    for w in 0..weeks {
        let from = first + w * WEEK;
        out.push(week(db, usage.as_deref(), project, from, from + WEEK)?);
    }
    Ok(Report { project: project.into(), weeks: out, note: NOTE })
}

fn week(db: &Db, usage: Option<&[crate::usage::Attributed]>, project: &str, from: i64, to: i64) -> Result<Week> {
    let c = &db.conn;
    let in_project = "SELECT key FROM issue WHERE project = ?1";
    // Each issue once, at its last land this week.
    let landed_at: Vec<(String, i64)> = c
        .prepare(&format!("SELECT issue_key, MAX(ts) FROM event WHERE type = 'landed' AND ts >= ?2 AND ts < ?3 AND issue_key IN ({in_project}) GROUP BY issue_key"))?
        .query_map(rusqlite::params![project, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let keys: Vec<&str> = landed_at.iter().map(|(k, _)| k.as_str()).collect();

    let mut rows = Vec::new();
    let mut accepted = Vec::new();
    let mut landed = Vec::new();
    for (key, at) in &landed_at {
        // In the order they ran, so a pass is checked against the review before it, as submit's
        // `reused_pass` does.
        let mut s = c.prepare("SELECT verdict, ts, findings, reviewer, COALESCE(diff_id, ''), rules FROM review WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts, id")?;
        let mut last: Option<(String, String, String, Option<String>)> = None;
        for r in s.query_map([key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, (r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?)))
        })? {
            let (verdict, ts, findings, (reviewer, diff, rules)) = r?;
            let reused = verdict == "pass"
                && last.as_ref().is_some_and(|(by, v, d, ru)| {
                    let before = nitely_core::submit_rules::LastReview { reviewer: by, verdict: v, diff_id: d, rules: ru.as_deref() };
                    rules.as_deref().is_some_and(|now| nitely_core::submit_rules::reusable(&before, &diff, now, Some(&reviewer)))
                });
            if !reviewer.is_empty() {
                last = Some((reviewer, verdict.clone(), diff, rules));
            }
            let findings: Vec<serde_json::Value> = findings.and_then(|f| serde_json::from_str(&f).ok()).unwrap_or_default();
            let findings = findings
                .iter()
                .map(|f| (f["priority"].as_i64().unwrap_or(9), f["text"].as_str().unwrap_or("").to_string(), f["carried"].as_bool().unwrap_or(false)))
                .collect();
            rows.push(ReviewRow { issue: key.clone(), verdict, ts, findings, reused });
        }
        let mut s = c.prepare("SELECT priority, reason FROM accepted_finding WHERE issue_key = ?1 COLLATE NOCASE")?;
        for a in s.query_map([key], |r| Ok(Acceptance { issue: key.clone(), priority: r.get(0)?, reason: r.get(1)? }))? {
            accepted.push(a?);
        }
        let claimed: Option<i64> = c.query_row("SELECT MIN(ts) FROM event WHERE type = 'claimed' AND issue_key = ?1", [key], |r| r.get(0))?;
        let submits = c
            .prepare("SELECT queued_at, ended_at FROM submit_job WHERE issue_key = ?1 AND state != 'superseded' AND queued_at <= ?2 ORDER BY queued_at")?
            .query_map(rusqlite::params![key, at], |r| Ok(Submit { queued: r.get(0)?, ended: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?;
        landed.push(Landed { issue: key.clone(), landed: *at, claimed, submits });
    }
    let (reviews, blocking) = reviews(&keys, &rows);

    let runs: Vec<(String, i64, i64, i64)> = c
        .prepare("SELECT kind, queued_at, started_at, ended_at FROM lane_run WHERE project = ?1 AND queued_at >= ?2 AND queued_at < ?3 AND started_at IS NOT NULL AND ended_at IS NOT NULL")?
        .query_map(rusqlite::params![project, from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut land_stops = BTreeMap::new();
    let mut s = c.prepare(&format!("SELECT summary FROM event WHERE type = 'land_failed' AND ts >= ?2 AND ts < ?3 AND issue_key IN ({in_project})"))?;
    for summary in s.query_map(rusqlite::params![project, from, to], |r| r.get::<_, Option<String>>(0))? {
        *land_stops.entry(stop_cause(&summary?.unwrap_or_default()).to_string()).or_insert(0) += 1;
    }

    let (asks, about): (usize, usize) = c.query_row(
        &format!("SELECT COUNT(*), COUNT(finding) FROM ask WHERE asked_at >= ?2 AND asked_at < ?3 AND issue_key IN ({in_project})"),
        rusqlite::params![project, from, to],
        |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
    )?;

    let tokens = usage.map(|records| {
        let of: HashMap<&str, ()> = keys.iter().map(|k| (*k, ())).collect();
        let spent: u64 = records.iter().filter(|r| r.issue.as_deref().is_some_and(|i| of.contains_key(i))).map(|r| r.usage.tokens()).sum();
        let mut context: Vec<i64> = records
            .iter()
            .filter(|r| r.project.as_deref() == Some(project) && r.usage.ts >= from && r.usage.ts < to)
            .map(|r| (r.usage.input + r.usage.cache_read + r.usage.cache_write) as i64)
            .collect();
        Tokens { per_landed: (!keys.is_empty()).then(|| spent / keys.len() as u64), calls: context.len(), median_context: median(&mut context).map(|m| m as u64) }
    });

    Ok(Week {
        week: chrono::DateTime::from_timestamp(from, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
        landed: keys.len(),
        findings: findings(blocking, &keys, &accepted),
        reviews,
        lanes: lanes(&runs),
        land_stops,
        asks: Asks { asks, about_findings: about, per_landed: ratio(asks, keys.len()) },
        issues: issue_times(&landed),
        tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(issue: &str, verdict: &str, ts: i64, findings: &[(i64, &str, bool)]) -> ReviewRow {
        ReviewRow { issue: issue.into(), verdict: verdict.into(), ts, findings: findings.iter().map(|(p, t, c)| (*p, t.to_string(), *c)).collect(), reused: false }
    }

    /// Two landed issues: rounds count per issue, a finding raised again or carried is not new, P2s
    /// never block, an unlanded issue and a batched placeholder are left out; an acceptance naming
    /// another issue is a split; a pass carried over unreviewed is no review; times are medians per
    /// issue.
    #[test]
    fn a_week_counts_rounds_new_blocking_findings_and_issue_times() {
        let rows = [
            review("APP-1", "changes", 10, &[(1, "a", false), (2, "minor", false)]),
            review("APP-1", "batched", 15, &[]),
            review("APP-1", "changes", 20, &[(1, "a", false), (0, "b", false)]),
            review("APP-1", "pass", 30, &[(1, "b", true)]),
            ReviewRow { reused: true, ..review("APP-1", "pass", 40, &[(1, "b", true)]) },
            review("APP-2", "pass", 12, &[]),
            review("APP-3", "changes", 12, &[(0, "x", false)]),
        ];
        let (r, blocking) = reviews(&["APP-1", "APP-2"], &rows);
        assert_eq!((r.reviews, r.per_landed, r.changes_rate, blocking), (4, Some(2.0), Some(0.5), 2));
        let by: Vec<(usize, usize, usize)> = r.by_round.iter().map(|x| (x.round, x.reviews, x.new_blocking)).collect();
        assert_eq!(by, [(1, 2, 1), (2, 1, 1), (3, 1, 0)]);

        let accepted = [
            Acceptance { issue: "APP-1".into(), priority: 1, reason: "a trade-off we chose".into() },
            Acceptance { issue: "APP-1".into(), priority: 0, reason: "APP-9 takes it".into() },
            Acceptance { issue: "APP-3".into(), priority: 1, reason: "not landed".into() },
        ];
        assert_eq!(findings(blocking, &["APP-1", "APP-2"], &accepted), Findings { blocking: 2, accepted: 1, split: 1, share: Some(1.0) });

        let t = issue_times(&[
            Landed { issue: "APP-1".into(), landed: 1000, claimed: Some(0), submits: vec![Submit { queued: 100, ended: Some(200) }, Submit { queued: 500, ended: Some(600) }] },
            Landed { issue: "APP-2".into(), landed: 400, claimed: Some(100), submits: vec![Submit { queued: 300, ended: Some(350) }] },
        ]);
        assert_eq!(t, IssueTimes { claim_to_first_submit: Some(150), submit_rounds: Some(1.5), fixing_between_rounds: Some(150), last_submit_to_landed: Some(300) });

        assert_eq!(stop_cause("Land stopped: the branch changed since its review (reviewed 1, now 2); submit again"), "changed_since_review");
        assert_eq!(stop_cause("Land stopped: the rebase of b onto main stopped on conflicts in a.rs"), "conflict");
        assert_eq!(stop_cause("Land stopped: the gate failed at `cargo test`"), "gate");
        let l = lanes(&[("gate".into(), 0, 60, 160), ("gate".into(), 0, 0, 40), ("run".into(), 5, 5, 6)]);
        assert_eq!((l[0].kind.as_str(), l[0].runs, l[0].wait_secs, l[0].run_secs), ("gate", 2, 60, 140));
        // 2026-10-27 was a Tuesday.
        assert_eq!(week_start(1_793_136_000), 1_792_972_800);
    }
}
