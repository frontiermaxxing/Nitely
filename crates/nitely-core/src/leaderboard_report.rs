//! The weekly Leaderboard report (NIT-443): the exact JSON that is shared once a week, and the
//! same JSON the opt-in page previews. Counts and rates only: a schema version, the display name
//! and links the person typed, the ISO week, how many projects count (a number, never their
//! names) and the stats engine's numbers. Never code, paths, issue keys or text, repo, project or
//! branch names, prompts, identity, the time zone or an IP.
//!
//! The builder copies numbers field by field out of `Leaderboard`, so a field added there stays
//! on this machine until it is added here on purpose, and `Report` refuses unknown fields when it
//! is read back (the service checks reports with it).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::leaderboard::{Leaderboard, PeriodStats, Spread, WeekPoint};

/// The report's schema version. Raise it for any change to what a report holds.
pub const SCHEMA: u32 = 1;

/// The vendors a report names; any other comes out as `other`.
pub const VENDORS: &[&str] = &["claude", "codex", "cursor", "gemini", "copilot"];

const MAX_NAME: usize = 40;
const MAX_URL: usize = 200;

/// What the person typed on the opt-in page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profile {
    pub display_name: String,
    /// A GitHub user name, or its https://github.com/ address.
    pub github: Option<String>,
    /// An X handle, with or without @, or its https://x.com/ address.
    pub x: Option<String>,
    /// An https address.
    pub website: Option<String>,
}

/// The shared report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub schema: u32,
    pub display_name: String,
    pub links: Links,
    /// The ISO week of the person's this week, like `2026-W43`.
    pub week: String,
    /// How many projects count.
    pub projects: u64,
    /// `week`, `30d` and `all`.
    pub periods: Vec<Period>,
    /// Landed changes per week, the 26 weeks up to `week`, oldest first.
    pub weekly: Vec<u64>,
    pub records: Records,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Links {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub period: String,
    pub landed: u64,
    pub landed_per_day: f64,
    pub reviewed: u64,
    pub first_pass_rate: Option<f64>,
    pub lead_time: Option<Durations>,
    pub agent_hours: f64,
    pub tokens: u64,
    pub inbox_reply: Option<Durations>,
    pub steadiest_week: Option<Week>,
    pub vendors: Vec<Vendor>,
    pub review_rounds: Option<Rounds>,
}

/// Seconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Durations {
    pub median: i64,
    pub p90: i64,
    pub count: u64,
}

/// A week, by its numbers alone: no date, so nothing tells the time zone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Week {
    pub landed: u64,
    pub active_days: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Vendor {
    pub vendor: String,
    pub landed: u64,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rounds {
    pub mean: f64,
    pub max: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub best_day: u64,
    pub best_week: u64,
    pub longest_streak_days: u64,
    /// The shortest lead time, in seconds (never which change it was).
    pub fastest_lead: Option<i64>,
}

/// The report for `board`, which counted `projects` projects, shared as `profile`.
pub fn build(board: &Leaderboard, projects: u64, profile: &Profile) -> Result<Report> {
    let this_week = board.periods.iter().find(|p| p.period == "week").map(|p| p.from).unwrap_or(board.generated_at);
    Ok(Report {
        schema: SCHEMA,
        display_name: display_name(&profile.display_name)?,
        links: links(profile)?,
        week: iso_week(this_week),
        projects,
        periods: board.periods.iter().map(period).collect(),
        weekly: board.weekly.iter().map(|w| w.landed).collect(),
        records: Records {
            best_day: board.records.best_day.as_ref().map(|d| d.landed).unwrap_or(0),
            best_week: board.records.best_week.as_ref().map(|w| w.landed).unwrap_or(0),
            longest_streak_days: board.records.longest_streak_days,
            fastest_lead: board.records.fastest_lead.as_ref().map(|f| f.seconds),
        },
    })
}

/// The report now, over `projects` (empty: every project), as `profile`.
pub fn report_now(config: &crate::Config, db: &crate::db::Db, projects: &[String], profile: &Profile) -> Result<Report> {
    let board = crate::leaderboard::stats_now(config, db, projects)?;
    let counted = if projects.is_empty() { db.projects()?.len() } else { board.projects.len() };
    build(&board, counted as u64, profile)
}

/// Why `profile` can't be shared as it is, if it can't.
pub fn check_profile(profile: &Profile) -> Result<()> {
    display_name(&profile.display_name)?;
    links(profile).map(|_| ())
}

fn period(p: &PeriodStats) -> Period {
    let durations = |s: &Option<Spread>| s.as_ref().map(|s| Durations { median: s.median, p90: s.p90, count: s.count });
    let mut vendors: Vec<Vendor> = Vec::new();
    for v in &p.vendors {
        let name = if VENDORS.contains(&v.vendor.as_str()) { v.vendor.as_str() } else { "other" };
        match vendors.iter_mut().find(|e| e.vendor == name) {
            Some(e) => e.landed += v.landed,
            None => vendors.push(Vendor { vendor: name.to_string(), landed: v.landed, share: 0.0 }),
        }
    }
    for v in &mut vendors {
        v.share = round(v.landed as f64 / p.landed.max(1) as f64, 3);
    }
    vendors.sort_by(|a, b| b.landed.cmp(&a.landed).then(a.vendor.cmp(&b.vendor)));
    Period {
        period: p.period.clone(),
        landed: p.landed,
        landed_per_day: round(p.landed_per_day, 2),
        reviewed: p.reviewed,
        first_pass_rate: p.first_pass_rate.map(|r| round(r, 3)),
        lead_time: durations(&p.lead_time),
        agent_hours: round(p.agent_hours, 1),
        tokens: p.tokens,
        inbox_reply: durations(&p.inbox_reply),
        steadiest_week: p.steadiest_week.as_ref().map(|w: &WeekPoint| Week { landed: w.landed, active_days: w.active_days }),
        vendors,
        review_rounds: p.review_rounds.as_ref().map(|r| Rounds { mean: round(r.mean, 2), max: r.max }),
    }
}

fn round(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

/// The ISO week of a local Monday 00:00 given in UTC seconds. That instant falls between Sunday
/// 10:00 and Monday 12:00 UTC for every offset from -12 to +14 hours, so 14 hours on is always
/// in the local Monday's ISO week, whatever the offset was.
fn iso_week(monday: i64) -> String {
    use chrono::Datelike;
    let day = chrono::DateTime::from_timestamp(monday + 14 * 3600, 0).unwrap_or_default().date_naive();
    let w = day.iso_week();
    format!("{}-W{:02}", w.year(), w.week())
}

fn display_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("choose a display name");
    }
    if name.chars().count() > MAX_NAME {
        bail!("a display name has at most {MAX_NAME} characters");
    }
    if name.chars().any(char::is_control) {
        bail!("a display name can't hold control characters");
    }
    if name.contains('@') {
        bail!("a display name can't hold @: use a name, not an email address");
    }
    Ok(name.to_string())
}

fn links(p: &Profile) -> Result<Links> {
    let given = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty());
    let handle = |v: &str, prefixes: &[&str], max: usize, ok: fn(char) -> bool, what: &str| -> Result<String> {
        let mut h = v.trim_start_matches('@');
        for prefix in prefixes {
            if let Some(rest) = h.strip_prefix(prefix) {
                h = rest.trim_end_matches('/');
            }
        }
        if h.is_empty() || h.len() > max || !h.chars().all(ok) {
            bail!("{what} isn't a valid user name");
        }
        Ok(h.to_string())
    };
    let github = given(&p.github)
        .map(|v| handle(v, &["https://github.com/", "http://github.com/", "github.com/"], 39, |c| c.is_ascii_alphanumeric() || c == '-', "the GitHub link"))
        .transpose()?
        .map(|h| format!("https://github.com/{h}"));
    let x = given(&p.x)
        .map(|v| handle(v, &["https://x.com/", "http://x.com/", "x.com/", "https://twitter.com/", "twitter.com/"], 15, |c| c.is_ascii_alphanumeric() || c == '_', "the X link"))
        .transpose()?
        .map(|h| format!("https://x.com/{h}"));
    let website = given(&p.website).map(website).transpose()?;
    Ok(Links { github, x, website })
}

/// An https address with a host, no user name or password, and nothing outside printable ASCII.
pub fn website(v: &str) -> Result<String> {
    let Some(rest) = v.strip_prefix("https://") else { bail!("the website must start with https://") };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if v.len() > MAX_URL || !v.chars().all(|c| c.is_ascii_graphic()) || host.is_empty() || host.contains('@') || !host.contains('.') {
        bail!("the website isn't a valid https address");
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::leaderboard::{stats, Inputs};
    use crate::usage::{Attributed, Usage};
    use rusqlite::params;

    /// Monday 2026-10-12 00:00 UTC.
    const MONDAY: i64 = 1_791_763_200;
    const DAY: i64 = 86_400;

    fn profile() -> Profile {
        Profile { display_name: "Night Owl".into(), github: Some("octocat".into()), x: Some("@octo_cat".into()), website: Some("https://example.com/me".into()) }
    }

    /// A database full of distinctive names, paths and text: none of it reaches the report, only
    /// counts and rates do.
    #[test]
    fn nothing_private_gets_in() {
        let base = crate::paths::unique_temp("nitely-leaderboard-report", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("QZX", "Quokkazebra Secret Project").unwrap();
        let secrets = [
            "QZX",
            "quokkazebra",
            "C:/Code/quokkazebra-private",
            "claude/qzx-7-zanzibar-branch",
            "zanzibar",
            "marmalade-prompt",
            "fennelmodel",
            "s-sessionid-9f3e",
            "sha-c0ffee1",
            "wombatvendor",
            "pangolin.example",
        ];
        let event = |ts: i64, actor: &str, key: &str, kind: &str, summary: &str| {
            db.conn
                .execute(
                    "INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![ts, actor, key, kind, summary, r#"{"path":"C:/Code/quokkazebra-private","prompt":"marmalade-prompt"}"#],
                )
                .unwrap();
        };
        let review = |ts: i64, key: &str, verdict: &str| {
            db.conn
                .execute(
                    "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, ts) VALUES (?1, 'zanzibar', 'sha-c0ffee1', 'codex', ?2, 'agent', ?3)",
                    params![key, verdict, ts],
                )
                .unwrap();
        };
        for (n, vendor) in ["claude", "codex", "wombatvendor"].iter().enumerate() {
            let key = format!("QZX-{}", 7 + n);
            let start = MONDAY + 6 * 3600 + n as i64 * DAY;
            event(start, &format!("{vendor}:s-sessionid-9f3e"), &key, "worktree", "Started quokkazebra in C:/Code/quokkazebra-private");
            review(start + 600, &key, if n == 0 { "changes" } else { "pass" });
            event(start + 3600, "cli", &key, "landed", &format!("Landed {vendor}/qzx-7-zanzibar-branch on main at sha-c0ffee1"));
        }
        db.conn
            .execute(
                "INSERT INTO ask (issue_key, asked_by, question, asked_at, answered_at, closed) VALUES ('QZX-7', 'claude:s-sessionid-9f3e', 'marmalade-prompt about zanzibar?', ?1, ?2, 1)",
                params![MONDAY, MONDAY + 900],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('s-sessionid-9f3e', 'claude', 'C:/Code/quokkazebra-private', ?1, ?2, 'ended')",
                params![MONDAY + 6 * 3600, MONDAY + 9 * 3600],
            )
            .unwrap();
        let tokens = vec![Attributed {
            usage: Usage {
                vendor: "claude",
                session: "s-sessionid-9f3e".into(),
                ts: MONDAY + 6 * 3600 + 60,
                model: "fennelmodel".into(),
                input: 4_200,
                output: 800,
                cache_read: 0,
                cache_write: 0,
                cwd: "C:/Code/quokkazebra-private".into(),
                branch: "claude/qzx-7-zanzibar-branch".into(),
            },
            project: Some("QZX".into()),
            issue: Some("QZX-7".into()),
        }];
        let project_of = |_: &str| Some("QZX".to_string());
        let projects = vec!["qzx".to_string()];
        let inputs = Inputs { now: MONDAY + 3 * DAY, utc_offset: -5 * 3600, projects: &projects, tokens: &tokens, project_of_dir: &project_of };
        let board = stats(&db, &inputs).unwrap();
        assert_eq!(board.records.fastest_lead.as_ref().map(|f| f.issue.as_str()), Some("QZX-7"), "the engine itself knows the key");

        let report = build(&board, 1, &profile()).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        let lower = json.to_lowercase();
        for secret in secrets {
            assert!(!lower.contains(&secret.to_lowercase()), "{secret:?} got into the report: {json}");
        }
        // Nor anything like a time of day: the week is a label, and no timestamp is sent.
        assert!(!json.contains(&MONDAY.to_string()[..6]), "a timestamp got in: {json}");
        let week = &report.periods[0];
        assert_eq!((week.landed, week.tokens, week.agent_hours, week.first_pass_rate), (3, 5_000, 3.0, Some(0.667)));
        assert_eq!(week.vendors.iter().map(|v| v.vendor.as_str()).collect::<Vec<_>>(), ["claude", "codex", "other"]);
        assert_eq!(report.projects, 1);
        assert_eq!(report.links.github.as_deref(), Some("https://github.com/octocat"));
        assert_eq!(report.links.x.as_deref(), Some("https://x.com/octo_cat"));

        // What is sent reads back as the same report, and a field the schema lacks is refused.
        assert_eq!(serde_json::from_str::<Report>(&json).unwrap(), report);
        let extra = json.replacen("\"schema\":1", "\"schema\":1,\"path\":\"x\"", 1);
        assert!(serde_json::from_str::<Report>(&extra).is_err());
        std::fs::remove_dir_all(base).ok();
    }

    /// The week label is the person's own week whatever their offset from UTC.
    #[test]
    fn the_week_is_the_local_iso_week() {
        for offset in [-12 * 3600, -5 * 3600, 0, 9 * 3600, 14 * 3600] {
            assert_eq!(iso_week(MONDAY - offset), "2026-W42", "offset {offset}");
        }
        assert_eq!(iso_week(MONDAY + 7 * DAY), "2026-W43");
    }

    #[test]
    fn the_profile_is_checked() {
        let with = |f: fn(&mut Profile)| {
            let mut p = profile();
            f(&mut p);
            check_profile(&p)
        };
        assert!(with(|p| p.display_name = "  ".into()).is_err());
        assert!(with(|p| p.display_name = "me@example.com".into()).is_err());
        assert!(with(|p| p.display_name = "x".repeat(41)).is_err());
        assert!(with(|p| p.github = Some("https://github.com/a/b".into())).is_err());
        assert!(with(|p| p.website = Some("http://example.com".into())).is_err());
        assert!(with(|p| p.website = Some("https://user@example.com".into())).is_err());
        assert!(with(|p| p.website = Some("https://example.com/a b".into())).is_err());
        assert!(with(|p| p.github = Some("https://github.com/octocat/".into())).is_ok());
    }
}
