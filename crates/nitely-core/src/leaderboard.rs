//! The Leaderboard's numbers (NIT-441): what you landed and how, per period, from the local
//! database alone. Read-only, with no network. The report (NIT-443) and hours saved (NIT-442)
//! build on `Leaderboard`.
//!
//! A change is a landing: a `landed` event of an issue of the chosen projects. Its vendor is the
//! branch it landed from (`claude/…`, `codex/…`), else whoever started it; it started at the
//! issue's first `worktree` or `claimed` event. Its reviews are the `review` rows recorded before
//! it landed: the first one's verdict says whether it passed on the first pass, and their count is
//! its review rounds. A landing with no review (a YOLO land) counts toward neither.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;
use crate::usage::Attributed;

const DAY: i64 = 86_400;
const WEEK: i64 = 7 * DAY;

/// How many weeks the landed series covers, the current one last.
pub const SERIES_WEEKS: usize = 26;

/// What the numbers are computed over.
pub struct Inputs<'a> {
    /// Now, in seconds since the epoch.
    pub now: i64,
    /// The person's local time's offset from UTC, in seconds: days and weeks (Monday first) are
    /// theirs.
    pub utc_offset: i64,
    /// The projects counted (issue prefixes, any case); empty counts every project.
    pub projects: &'a [String],
    /// Token records (`usage::scan`), each with its project.
    pub tokens: &'a [Attributed],
    /// The project a session's folder belongs to (`Config::repo_for_dir`).
    pub project_of_dir: &'a dyn Fn(&str) -> Option<String>,
}

/// Every number the Leaderboard shows.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Leaderboard {
    pub generated_at: i64,
    /// The projects counted, upper case; empty means every project.
    pub projects: Vec<String>,
    /// This week (since Monday 00:00 local), the last 30 days, and all time.
    pub periods: Vec<PeriodStats>,
    /// Landed changes per week, the last `SERIES_WEEKS` weeks, oldest first, this week last.
    pub weekly: Vec<WeekPoint>,
    /// Personal records, over all time.
    pub records: Records,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PeriodStats {
    /// `week`, `30d` or `all`.
    pub period: String,
    pub from: i64,
    pub to: i64,
    pub landed: u64,
    /// Landed per day over the period's days (all time: since the first landing).
    pub landed_per_day: f64,
    /// Of the landed changes that were reviewed, the share whose first review passed (0 to 1).
    pub first_pass_rate: Option<f64>,
    /// How many landed changes were reviewed.
    pub reviewed: u64,
    /// Started to landed, in seconds.
    pub lead_time: Option<Spread>,
    /// Hours agents' sessions ran in the period (every session counts as automated).
    pub agent_hours: f64,
    /// Tokens spent (new input, cache writes and output; `Usage::tokens`).
    pub tokens: u64,
    /// Asked to answered, for Inbox questions answered in the period, in seconds.
    pub inbox_reply: Option<Spread>,
    /// The week with landings on the most days.
    pub steadiest_week: Option<WeekPoint>,
    /// Landed changes per vendor, most first.
    pub vendors: Vec<VendorShare>,
    /// Review rounds per reviewed landed change.
    pub review_rounds: Option<Rounds>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Spread {
    pub median: i64,
    pub p90: i64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rounds {
    pub mean: f64,
    pub max: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WeekPoint {
    /// Monday 00:00 local, in seconds since the epoch.
    pub week_start: i64,
    pub landed: u64,
    /// Days of the week with a landing.
    pub active_days: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VendorShare {
    /// `claude`, `codex`, `cursor`, … or `other`.
    pub vendor: String,
    pub landed: u64,
    /// Of all landed in the period (0 to 1).
    pub share: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Records {
    /// The day with the most landings: its start (local) and count.
    pub best_day: Option<DayCount>,
    pub best_week: Option<WeekPoint>,
    /// The most days in a row with a landing.
    pub longest_streak_days: u64,
    /// The landing with the shortest lead time.
    pub fastest_lead: Option<Fastest>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DayCount {
    pub day_start: i64,
    pub landed: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fastest {
    pub issue: String,
    pub seconds: i64,
}

/// One landed change, as the numbers read it.
#[derive(Debug, Clone)]
struct Change {
    issue: String,
    landed_at: i64,
    vendor: String,
    lead: Option<i64>,
    /// Review verdicts before it landed, oldest first.
    reviews: Vec<String>,
}

/// The numbers for `inputs`, read from `db`.
pub fn stats(db: &Db, inputs: &Inputs) -> Result<Leaderboard> {
    let projects: Vec<String> = inputs.projects.iter().map(|p| p.to_uppercase()).collect();
    let counted = |key_or_project: &str| {
        let project = key_or_project.split('-').next().unwrap_or("").to_uppercase();
        projects.is_empty() || projects.contains(&project)
    };
    let changes = changes(db, &counted)?;
    let week_start = |t: i64| {
        let local = t + inputs.utc_offset;
        // The epoch was a Thursday: Monday is three days on.
        let days = local.div_euclid(DAY);
        (days - (days + 3).rem_euclid(7)) * DAY - inputs.utc_offset
    };
    let day_start = |t: i64| (t + inputs.utc_offset).div_euclid(DAY) * DAY - inputs.utc_offset;

    let this_week = week_start(inputs.now);
    let first_landing = changes.iter().map(|c| c.landed_at).min();
    let spans = [
        ("week", this_week),
        ("30d", inputs.now - 30 * DAY),
        ("all", first_landing.map(day_start).unwrap_or(inputs.now).min(inputs.now)),
    ];
    let asks = asks(db, &counted)?;
    let sessions = sessions(db, inputs, &counted)?;
    let mut periods = Vec::new();
    for (name, from) in spans {
        let to = inputs.now;
        let inside: Vec<&Change> = changes.iter().filter(|c| c.landed_at >= from && c.landed_at <= to).collect();
        let landed = inside.len() as u64;
        let days = (((to - from) as f64) / DAY as f64).max(1.0);
        let reviewed: Vec<&&Change> = inside.iter().filter(|c| !c.reviews.is_empty()).collect();
        let first_pass_rate = (!reviewed.is_empty()).then(|| reviewed.iter().filter(|c| c.reviews[0] == "pass").count() as f64 / reviewed.len() as f64);
        let review_rounds = (!reviewed.is_empty()).then(|| Rounds {
            mean: reviewed.iter().map(|c| c.reviews.len() as f64).sum::<f64>() / reviewed.len() as f64,
            max: reviewed.iter().map(|c| c.reviews.len() as u64).max().unwrap_or(0),
        });
        let lead_time = spread(inside.iter().filter_map(|c| c.lead).collect());
        let inbox_reply = spread(asks.iter().filter(|(answered, _)| *answered >= from && *answered <= to).map(|(_, took)| *took).collect());
        let agent_hours = sessions.iter().map(|(start, end)| (end.min(&to) - start.max(&from)).max(0)).sum::<i64>() as f64 / 3600.0;
        let tokens = inputs
            .tokens
            .iter()
            .filter(|t| t.usage.ts >= from && t.usage.ts <= to)
            .filter(|t| projects.is_empty() || t.project.as_deref().is_some_and(|p| projects.contains(&p.to_uppercase())))
            .map(|t| t.usage.tokens())
            .sum();
        let mut by_vendor: BTreeMap<String, u64> = BTreeMap::new();
        for c in &inside {
            *by_vendor.entry(c.vendor.clone()).or_default() += 1;
        }
        let mut vendors: Vec<VendorShare> =
            by_vendor.into_iter().map(|(vendor, n)| VendorShare { vendor, landed: n, share: n as f64 / landed.max(1) as f64 }).collect();
        vendors.sort_by(|a, b| b.landed.cmp(&a.landed).then(a.vendor.cmp(&b.vendor)));
        let steadiest_week = weeks(inside.iter().map(|c| c.landed_at), &week_start, &day_start)
            .into_iter()
            .max_by(|a, b| a.active_days.cmp(&b.active_days).then(a.landed.cmp(&b.landed)).then(a.week_start.cmp(&b.week_start)));
        periods.push(PeriodStats {
            period: name.to_string(),
            from,
            to,
            landed,
            landed_per_day: landed as f64 / days,
            first_pass_rate,
            reviewed: reviewed.len() as u64,
            lead_time,
            agent_hours,
            tokens,
            inbox_reply,
            steadiest_week,
            vendors,
            review_rounds,
        });
    }

    let all_weeks = weeks(changes.iter().map(|c| c.landed_at), &week_start, &day_start);
    let by_start: HashMap<i64, &WeekPoint> = all_weeks.iter().map(|w| (w.week_start, w)).collect();
    let weekly = (0..SERIES_WEEKS as i64)
        .rev()
        .map(|back| {
            let start = week_start(this_week - back * WEEK + DAY);
            by_start.get(&start).map(|w| (*w).clone()).unwrap_or(WeekPoint { week_start: start, landed: 0, active_days: 0 })
        })
        .collect();

    let mut per_day: BTreeMap<i64, u64> = BTreeMap::new();
    for c in &changes {
        *per_day.entry(day_start(c.landed_at)).or_default() += 1;
    }
    let mut streak = 0u64;
    let mut longest = 0u64;
    let mut last: Option<i64> = None;
    for &day in per_day.keys() {
        // A day is a calendar day: a day across a DST change is still the next one.
        streak = match last {
            Some(prev) if (day - prev - DAY).abs() < 2 * 3600 => streak + 1,
            _ => 1,
        };
        longest = longest.max(streak);
        last = Some(day);
    }
    let records = Records {
        best_day: per_day.iter().max_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0))).map(|(d, n)| DayCount { day_start: *d, landed: *n }),
        best_week: all_weeks.iter().max_by(|a, b| a.landed.cmp(&b.landed).then(a.week_start.cmp(&b.week_start))).cloned(),
        longest_streak_days: longest,
        fastest_lead: changes.iter().filter_map(|c| c.lead.map(|l| (l, c))).min_by_key(|(l, _)| *l).map(|(l, c)| Fastest { issue: c.issue.clone(), seconds: l }),
    };
    Ok(Leaderboard { generated_at: inputs.now, projects, periods, weekly, records })
}

/// The landed changes of the counted projects, oldest first.
fn changes(db: &Db, counted: &dyn Fn(&str) -> bool) -> Result<Vec<Change>> {
    let mut started: HashMap<String, (i64, String)> = HashMap::new();
    let mut stmt = db.conn.prepare("SELECT issue_key, ts, actor FROM event WHERE type IN ('worktree', 'claimed') AND issue_key IS NOT NULL ORDER BY ts, id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))? {
        let (key, ts, actor) = row?;
        started.entry(key).or_insert((ts, actor));
    }
    let mut reviews: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    let mut stmt = db.conn.prepare("SELECT issue_key, ts, verdict FROM review WHERE issue_key IS NOT NULL ORDER BY ts, id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))? {
        let (key, ts, verdict) = row?;
        reviews.entry(key).or_default().push((ts, verdict));
    }
    let mut out = Vec::new();
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    let mut stmt = db.conn.prepare("SELECT issue_key, ts, summary FROM event WHERE type = 'landed' AND issue_key IS NOT NULL ORDER BY ts, id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))? {
        let (issue, landed_at, summary) = row?;
        if !counted(&issue) || !seen.insert((issue.clone(), landed_at)) {
            continue;
        }
        let start = started.get(&issue);
        let vendor = summary
            .as_deref()
            .and_then(|s| s.strip_prefix("Landed "))
            .and_then(|s| s.split_once('/'))
            .map(|(v, _)| v.to_string())
            .or_else(|| start.and_then(|(_, actor)| actor.split_once(':').map(|(v, _)| v.to_string())))
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            .map(|v| v.to_lowercase())
            .unwrap_or_else(|| "other".to_string());
        let lead = start.map(|(ts, _)| landed_at - ts).filter(|l| *l >= 0);
        let reviews = reviews.get(&issue).map(|r| r.iter().filter(|(ts, _)| *ts <= landed_at).map(|(_, v)| v.clone()).collect()).unwrap_or_default();
        out.push(Change { issue, landed_at, vendor, lead, reviews });
    }
    Ok(out)
}

/// Answered Inbox questions of the counted projects: (answered at, seconds to answer).
fn asks(db: &Db, counted: &dyn Fn(&str) -> bool) -> Result<Vec<(i64, i64)>> {
    let mut stmt = db.conn.prepare("SELECT issue_key, asked_at, answered_at FROM ask WHERE answered_at IS NOT NULL AND asked_at IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (key, asked, answered) = row?;
        if key.as_deref().is_some_and(|k| counted(k)) && answered >= asked {
            out.push((answered, answered - asked));
        }
    }
    Ok(out)
}

/// Agent sessions of the counted projects: (started, last seen).
fn sessions(db: &Db, inputs: &Inputs, counted: &dyn Fn(&str) -> bool) -> Result<Vec<(i64, i64)>> {
    let mut stmt = db.conn.prepare("SELECT cwd, started_at, last_seen_at FROM session WHERE started_at IS NOT NULL")?;
    let rows: Vec<(Option<String>, i64, Option<i64>)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut project_of: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::new();
    for (cwd, start, seen) in rows {
        let Some(cwd) = cwd else { continue };
        let project = project_of.entry(cwd.clone()).or_insert_with(|| (inputs.project_of_dir)(&cwd)).clone();
        if project.is_some_and(|p| counted(&p)) {
            out.push((start, seen.unwrap_or(start).max(start)));
        }
    }
    Ok(out)
}

/// The weeks with a landing among `landed_at`.
fn weeks(landed_at: impl Iterator<Item = i64>, week_start: &dyn Fn(i64) -> i64, day_start: &dyn Fn(i64) -> i64) -> Vec<WeekPoint> {
    let mut by_week: BTreeMap<i64, (u64, HashSet<i64>)> = BTreeMap::new();
    for t in landed_at {
        let w = by_week.entry(week_start(t)).or_default();
        w.0 += 1;
        w.1.insert(day_start(t));
    }
    by_week.into_iter().map(|(week_start, (landed, days))| WeekPoint { week_start, landed, active_days: days.len() as u64 }).collect()
}

/// The median and p90 of `values`, None for none.
fn spread(mut values: Vec<i64>) -> Option<Spread> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    Some(Spread { median: at(0.5), p90: at(0.9), count: values.len() as u64 })
}

/// The numbers now, over `projects` (empty: every project): tokens from the agents' own session
/// files on this machine (`usage::scan`, counts only), sessions' projects from `config`.
pub fn stats_now(config: &crate::Config, db: &Db, projects: &[String]) -> Result<Leaderboard> {
    let tokens = crate::usage::scan(config, db, 0);
    let project_of_dir = |dir: &str| config.repo_for_dir(dir).and_then(|r| r.project.clone());
    stats(db, &Inputs { now: crate::db::now(), utc_offset: local_offset(), projects, tokens: &tokens, project_of_dir: &project_of_dir })
}

/// The local time's offset from UTC now, in seconds.
pub fn local_offset() -> i64 {
    i64::from(chrono::Local::now().offset().local_minus_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::Usage;
    use rusqlite::params;

    /// Monday 2026-10-12 00:00 UTC.
    const MONDAY: i64 = 1_791_763_200;

    fn db() -> (Db, std::path::PathBuf) {
        let base = crate::paths::unique_temp("nitely-leaderboard", "");
        std::fs::create_dir_all(&base).unwrap();
        (Db::open(&base.join("t.db")).unwrap(), base)
    }

    fn event(db: &Db, ts: i64, actor: &str, key: &str, kind: &str, summary: &str) {
        db.conn
            .execute("INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, ?2, ?3, ?4, ?5, '{}')", params![ts, actor, key, kind, summary])
            .unwrap();
    }

    fn review(db: &Db, ts: i64, key: &str, verdict: &str) {
        db.conn
            .execute("INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, ts) VALUES (?1, 'p', 'c', 'codex', ?2, 'agent', ?3)", params![key, verdict, ts])
            .unwrap();
    }

    /// One change: started at `start`, reviewed with `verdicts` an hour apart, landed at `landed`.
    fn change(db: &Db, key: &str, vendor: &str, start: i64, verdicts: &[&str], landed: i64) {
        event(db, start, &format!("{vendor}:s1"), key, "worktree", "Started");
        for (n, v) in verdicts.iter().enumerate() {
            review(db, start + 600 + n as i64 * 600, key, v);
        }
        event(db, landed, "cli", key, "landed", &format!("Landed {vendor}/{}-x on main at abc1234", key.to_lowercase()));
    }

    /// A known history: three NIT changes this week (two vendors, one reviewed twice, one YOLO),
    /// one the week before, one 40 days back, and one APP change left out by the project choice.
    #[test]
    fn the_numbers_of_a_known_history() {
        let (db, base) = db();
        let now = MONDAY + 3 * DAY + 12 * 3600; // Thursday noon
        change(&db, "NIT-1", "claude", MONDAY + 3600, &["pass"], MONDAY + 2 * 3600); // lead 1 h
        change(&db, "NIT-2", "codex", MONDAY + DAY, &["changes", "pass"], MONDAY + DAY + 4 * 3600); // lead 4 h, 2 rounds
        change(&db, "NIT-3", "claude", MONDAY + 2 * DAY, &[], MONDAY + 2 * DAY + 1800); // YOLO, lead 30 m
        change(&db, "NIT-4", "claude", MONDAY - 5 * DAY, &["pass"], MONDAY - 5 * DAY + 7200);
        change(&db, "NIT-5", "codex", MONDAY - 40 * DAY, &["changes"], MONDAY - 40 * DAY + 3600);
        change(&db, "APP-9", "claude", MONDAY + DAY, &["pass"], MONDAY + DAY + 60);
        // A review after the landing belongs to no round of it.
        review(&db, MONDAY + 2 * DAY, "NIT-1", "changes");
        db.conn
            .execute("INSERT INTO ask (issue_key, asked_by, question, asked_at, answered_at, closed) VALUES ('NIT-2', 'codex:s1', 'Which?', ?1, ?2, 1)", params![MONDAY + DAY, MONDAY + DAY + 900])
            .unwrap();
        db.conn
            .execute("INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('s1', 'claude', 'C:/Code/nitely', ?1, ?2, 'ended')", params![MONDAY, MONDAY + 2 * 3600])
            .unwrap();
        db.conn
            .execute("INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('s2', 'codex', 'C:/Code/app', ?1, ?2, 'ended')", params![MONDAY, MONDAY + 5 * 3600])
            .unwrap();
        let tokens = |ts: i64, project: &str, n: u64| Attributed {
            usage: Usage { vendor: "claude", session: "s1".into(), ts, model: "m".into(), input: n, output: 0, cache_read: 999, cache_write: 0, cwd: String::new(), branch: String::new() },
            project: Some(project.into()),
            issue: None,
        };
        let records = vec![tokens(MONDAY + 60, "NIT", 1_000), tokens(MONDAY - 40 * DAY, "NIT", 5_000), tokens(MONDAY + 60, "APP", 70)];
        let project_of = |dir: &str| Some(if dir.ends_with("nitely") { "NIT" } else { "APP" }.to_string());
        let only_nit = vec!["nit".to_string()];
        let inputs = Inputs { now, utc_offset: 0, projects: &only_nit, tokens: &records, project_of_dir: &project_of };
        let b = stats(&db, &inputs).unwrap();
        assert_eq!(b.projects, ["NIT"]);
        let week = &b.periods[0];
        assert_eq!((week.period.as_str(), week.from, week.landed), ("week", MONDAY, 3));
        assert!((week.landed_per_day - 3.0 / 3.5).abs() < 1e-9, "{}", week.landed_per_day);
        assert_eq!((week.reviewed, week.first_pass_rate), (2, Some(0.5)), "the YOLO land counts toward neither");
        assert_eq!(week.review_rounds, Some(Rounds { mean: 1.5, max: 2 }), "the review after NIT-1 landed is no round of it");
        assert_eq!(week.lead_time, Some(Spread { median: 3600, p90: 4 * 3600, count: 3 }));
        assert_eq!(week.inbox_reply, Some(Spread { median: 900, p90: 900, count: 1 }));
        assert_eq!(week.agent_hours, 2.0, "only NIT's session");
        assert_eq!(week.tokens, 1_000, "NIT's, cache reads apart");
        assert_eq!(
            week.vendors,
            [VendorShare { vendor: "claude".into(), landed: 2, share: 2.0 / 3.0 }, VendorShare { vendor: "codex".into(), landed: 1, share: 1.0 / 3.0 }]
        );
        assert_eq!(week.steadiest_week, Some(WeekPoint { week_start: MONDAY, landed: 3, active_days: 3 }));
        let month = &b.periods[1];
        assert_eq!(month.landed, 4, "NIT-5 is 40 days back");
        let all = &b.periods[2];
        assert_eq!((all.landed, all.tokens, all.first_pass_rate), (5, 6_000, Some(0.5)));
        assert_eq!(all.from, MONDAY - 40 * DAY, "all time starts the day of the first landing");

        assert_eq!(b.weekly.len(), SERIES_WEEKS);
        assert_eq!(b.weekly.last().unwrap(), &WeekPoint { week_start: MONDAY, landed: 3, active_days: 3 });
        assert_eq!(b.weekly[SERIES_WEEKS - 2], WeekPoint { week_start: MONDAY - WEEK, landed: 1, active_days: 1 });
        assert_eq!(b.weekly[0].week_start, MONDAY - 25 * WEEK);

        assert_eq!(b.records.longest_streak_days, 3, "Monday to Wednesday");
        assert_eq!(b.records.best_week, Some(WeekPoint { week_start: MONDAY, landed: 3, active_days: 3 }));
        assert_eq!(b.records.best_day.as_ref().map(|d| d.landed), Some(1));
        assert_eq!(b.records.fastest_lead, Some(Fastest { issue: "NIT-3".into(), seconds: 1800 }));

        // Every project: APP's change, session and tokens count too.
        let inputs = Inputs { projects: &[], ..inputs };
        let b = stats(&db, &inputs).unwrap();
        assert_eq!((b.periods[0].landed, b.periods[0].agent_hours, b.periods[0].tokens), (4, 7.0, 1_070));
        std::fs::remove_dir_all(base).ok();
    }

    /// Weeks are the person's: with a UTC-5 clock, Sunday 22:00 there is still last week.
    #[test]
    fn weeks_and_days_are_local() {
        let (db, base) = db();
        let offset = -5 * 3600;
        let local_monday = MONDAY - offset; // Monday 00:00 at UTC-5
        change(&db, "NIT-1", "claude", local_monday - 3 * 3600, &[], local_monday - 2 * 3600); // Sunday 22:00 local
        change(&db, "NIT-2", "claude", local_monday + 3600, &[], local_monday + 2 * 3600);
        let project_of = |_: &str| None;
        let inputs = Inputs { now: local_monday + DAY, utc_offset: offset, projects: &[], tokens: &[], project_of_dir: &project_of };
        let b = stats(&db, &inputs).unwrap();
        assert_eq!((b.periods[0].from, b.periods[0].landed), (local_monday, 1));
        assert_eq!(b.records.longest_streak_days, 2, "Sunday then Monday");
        assert_eq!(b.weekly[SERIES_WEEKS - 2].landed, 1);
        std::fs::remove_dir_all(base).ok();
    }

    /// An empty database has numbers too: zeros and nothing to rate.
    #[test]
    fn no_history_reads_as_zeros() {
        let (db, base) = db();
        let project_of = |_: &str| None;
        let b = stats(&db, &Inputs { now: MONDAY, utc_offset: 0, projects: &[], tokens: &[], project_of_dir: &project_of }).unwrap();
        assert!(b.periods.iter().all(|p| p.landed == 0 && p.first_pass_rate.is_none() && p.lead_time.is_none()));
        assert_eq!(b.records, Records::default());
        assert_eq!(b.weekly.len(), SERIES_WEEKS);
        std::fs::remove_dir_all(base).ok();
    }
}
