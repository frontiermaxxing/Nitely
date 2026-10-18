//! `nitely report`'s text form (NIT-358): the goal report read in a terminal in the morning, shaped
//! like the phone's (src/phone/Report.tsx) and grouped by the same rules as src/goals.ts: per goal
//! what is at risk, then its newest landings; what landed outside any goal by project; one line per
//! issue that needed you, what still waits first; and what triage answered. `--json` is the report
//! as it is.

use std::collections::HashMap;
use std::fmt::Write;

use chrono::{DateTime, Datelike, TimeZone};

use crate::goal_report::{Finish, Landed, Needed, Report};

/// Landings shown per goal, and needed issues shown, before "+N more" (unless `all`).
const SHOWN: usize = 3;
const NEEDS_SHOWN: usize = 5;
/// Longest a title runs on its line; a needed line's summary, after its time, issue and kind.
const WIDTH: usize = 90;
const NEED_WIDTH: usize = 70;

/// The places one issue needed you, as one line.
struct Group<'a> {
    key: Option<&'a str>,
    ts: i64,
    summary: &'a str,
    asked: usize,
    approvals: usize,
    open: usize,
}

/// One per issue, newest summary; what still waits first, then the newest. No issue: on its own.
fn group(needed: &[Needed]) -> Vec<Group<'_>> {
    let mut order: Vec<Group> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (i, n) in needed.iter().enumerate() {
        let id = n.issue_key.as_deref().map(str::to_uppercase).unwrap_or_else(|| format!("#{i}"));
        let g = *at.entry(id).or_insert_with(|| {
            order.push(Group { key: n.issue_key.as_deref(), ts: i64::MIN, summary: "", asked: 0, approvals: 0, open: 0 });
            order.len() - 1
        });
        let g = &mut order[g];
        if n.kind == "asked" {
            g.asked += 1;
        } else {
            g.approvals += 1;
        }
        g.open += n.open as usize;
        if n.ts >= g.ts {
            g.ts = n.ts;
            g.summary = &n.summary;
        }
    }
    order.sort_by(|a, b| (b.open > 0).cmp(&(a.open > 0)).then(b.ts.cmp(&a.ts)));
    order
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

fn kinds(asked: usize, approvals: usize) -> String {
    [(asked, "question"), (approvals, "approval")].iter().filter(|(n, _)| *n > 0).map(|(n, one)| plural(*n, one)).collect::<Vec<_>>().join(", ")
}

/// "You were needed 9 times on 4 issues: 4 questions, 5 approvals · 8 still wait", as Pulse says it.
fn needed_line(needed: &[Needed], issues: usize) -> String {
    let asked = needed.iter().filter(|n| n.kind == "asked").count();
    let open = needed.iter().filter(|n| n.open).count();
    let times = if needed.len() == 1 { "once".to_string() } else { format!("{} times", needed.len()) };
    let on = if issues < needed.len() { format!(" on {}", plural(issues, "issue")) } else { String::new() };
    let waiting = match open {
        0 => String::new(),
        n if n == needed.len() && n > 1 => " · all still wait".into(),
        1 => " · 1 still waits".into(),
        n => format!(" · {n} still wait"),
    };
    format!("You were needed {times}{on}: {}{waiting}", kinds(asked, needed.len() - asked))
}

/// "NIT 33 · APP 27": landings by the project in their keys, the most first.
fn by_project(landed: &[Landed]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for l in landed {
        let project = l.key.rsplit_once('-').map_or(l.key.as_str(), |(p, _)| p).to_string();
        match counts.iter_mut().find(|(p, _)| *p == project) {
            Some((_, n)) => *n += 1,
            None => counts.push((project, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    counts.iter().map(|(p, n)| format!("{p} {n}")).collect::<Vec<_>>().join(" · ")
}

/// Cut to `WIDTH` characters, on one line.
fn short(text: &str) -> String {
    cut(text, WIDTH)
}

fn cut(text: &str, width: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= width {
        return one;
    }
    format!("{}…", one.chars().take(width - 1).collect::<String>().trim_end())
}

fn clock<Tz: TimeZone>(t: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    t.format("%-I:%M %p").to_string()
}

/// "today", "tomorrow" or "~Oct 22", and whether that is after `target` (YYYY-MM-DD).
fn finish_line<Tz: TimeZone>(finish: &Finish, target: Option<&str>, now: &DateTime<Tz>) -> (String, bool)
where
    Tz::Offset: std::fmt::Display,
{
    let at = now.timezone().timestamp_opt(finish.at, 0).single().unwrap_or_else(|| now.clone());
    let days = (at.date_naive() - now.date_naive()).num_days();
    let label = match days {
        ..=0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        _ if at.year() != now.year() => format!("~{}", at.format("%b %-d, %Y")),
        _ => format!("~{}", at.format("%b %-d")),
    };
    let late = target.and_then(|t| chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()).is_some_and(|t| at.date_naive() > t);
    (format!("done {label} at this pace"), late)
}

/// The report as text, its times in `tz`. With `all`, nothing is folded into "+N more".
pub fn text<Tz: TimeZone>(r: &Report, tz: &Tz, all: bool) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = |ts: i64| tz.timestamp_opt(ts, 0).single().unwrap_or_else(|| tz.timestamp_opt(0, 0).unwrap());
    let now = at(r.now);
    let since = at(r.since);
    let mut out = String::new();
    let hours = ((r.now - r.since) as f64 / 3600.0).round() as i64;
    let day = if since.date_naive() == now.date_naive() { String::new() } else { format!("{} ", since.format("%a")) };
    let _ = writeln!(out, "Since {day}{} ({})", clock(&since), plural(hours.max(0) as usize, "hour"));

    let landed_lines = |out: &mut String, landed: &[Landed]| {
        let mut newest: Vec<&Landed> = landed.iter().collect();
        newest.sort_by(|a, b| b.ts.cmp(&a.ts));
        let shown = if all { newest.len() } else { SHOWN.min(newest.len()) };
        for l in &newest[..shown] {
            let _ = writeln!(out, "  {}  {}", l.key, short(&l.title));
        }
        if newest.len() > shown {
            let _ = writeln!(out, "  +{} more", newest.len() - shown);
        }
    };

    if r.goals.is_empty() {
        let _ = writeln!(out, "\nNo active goals.");
    }
    for g in &r.goals {
        let p = &g.goal.progress;
        let mut head = format!("{} landed · {}/{} done", g.landed.len(), p.done, p.total - p.dropped);
        if let Some(f) = &g.finish {
            let (line, late) = finish_line(f, g.goal.target_date.as_deref(), &now);
            head = format!("{head} · {line}{}", if late { ", after its target" } else { "" });
        }
        let _ = writeln!(out, "\n{}\n  {head}", short(&g.goal.title));
        for risk in &g.risks {
            let _ = writeln!(out, "  ! {}  {}", risk.key, risk.why);
        }
        landed_lines(&mut out, &g.landed);
    }
    if !r.other_landed.is_empty() {
        let projects = by_project(&r.other_landed);
        let several = projects.contains(" · ");
        let _ = writeln!(out, "\nOutside any goal: {} landed{}", r.other_landed.len(), if several { format!(" ({projects})") } else { String::new() });
        landed_lines(&mut out, &r.other_landed);
    }

    let groups = group(&r.needed);
    if r.needed.is_empty() {
        let _ = writeln!(out, "\nYou were not needed.");
    } else {
        let _ = writeln!(out, "\n{}", needed_line(&r.needed, groups.len()));
        let shown = if all { groups.len() } else { NEEDS_SHOWN.min(groups.len()) };
        for g in &groups[..shown] {
            let many = g.asked + g.approvals > 1;
            let what = if many { kinds(g.asked, g.approvals) } else if g.asked > 0 { "asked".into() } else { "approval".into() };
            let waits = match g.open {
                0 => String::new(),
                n if many && n < g.asked + g.approvals => format!(" · {n} wait"),
                _ => " · waits".into(),
            };
            let _ = writeln!(out, "  {:>8}  {}  {what}{waits}: {}", clock(&at(g.ts)), g.key.unwrap_or("-"), cut(g.summary, NEED_WIDTH));
        }
        if groups.len() > shown {
            let more = groups.len() - shown;
            let _ = writeln!(out, "  +{more} more {} (--all shows every one)", if more == 1 { "issue" } else { "issues" });
        }
    }
    if !r.triaged.is_empty() {
        let n = r.triaged.len();
        let _ = writeln!(out, "\nTriage answered {} without you", if n == 1 { "a question".to_string() } else { format!("{n} questions") });
    }
    out
}

/// `text` in this machine's time zone.
pub fn local(r: &Report, all: bool) -> String {
    text(r, &chrono::Local, all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_report::{GoalReport, Risk, Triaged};
    use chrono::FixedOffset;

    fn landed(key: &str, ts: i64) -> Landed {
        Landed { key: key.into(), title: format!("{key} title"), ts }
    }

    fn need(ts: i64, key: Option<&str>, kind: &'static str, open: bool, summary: &str) -> Needed {
        Needed { ts, kind, issue_key: key.map(Into::into), summary: summary.into(), open }
    }

    #[test]
    fn reads_like_the_phone_report() {
        let tz = FixedOffset::west_opt(5 * 3600).unwrap();
        // 2026-10-18 08:00 in that zone, and the night before from 21:00.
        let now = tz.with_ymd_and_hms(2026, 10, 18, 8, 0, 0).unwrap().timestamp();
        let since = now - 11 * 3600;
        let goal = crate::goals::Goal {
            id: 1,
            project: "APP".into(),
            title: "Invoices people can trust".into(),
            outcome: String::new(),
            target_date: Some("2026-10-19".into()),
            state: "active".into(),
            created_by: "human".into(),
            created_at: 0,
            updated_at: 0,
            progress: crate::goals::Progress { total: 9, done: 5, dropped: 1, open: 3 },
            rank: None,
            workers: None,
            working: 0,
            landed_today: 0,
        };
        let report = Report {
            since,
            now,
            goals: vec![GoalReport {
                goal,
                landed: (1..=5).map(|n| landed(&format!("APP-{n}"), since + n * 600)).collect(),
                risks: vec![Risk { key: "APP-9".into(), title: "Credit notes".into(), why: "stuck: the gate failed twice".into() }],
                finish: Some(Finish { at: now + 3 * 86400, per_day: 1.0 }),
            }],
            other_landed: vec![landed("APP-20", since), landed("WEB-3", since + 60), landed("WEB-4", since + 120)],
            needed: vec![
                need(since + 100, Some("APP-7"), "asked", false, "Which rounding?"),
                need(since + 200, Some("app-8"), "asked", true, "accept a finding? [P1] first"),
                need(since + 200, Some("APP-8"), "asked", true, "accept a finding? [P1] second"),
                need(since + 300, Some("APP-8"), "approval", false, "the change"),
            ],
            triaged: vec![Triaged { ts: since, issue_key: None, question: "q".into(), answer: "a".into() }],
        };
        let text = text(&report, &tz, false);
        let expected = "\
Since Sat 9:00 PM (11 hours)

Invoices people can trust
  5 landed · 5/8 done · done ~Oct 21 at this pace, after its target
  ! APP-9  stuck: the gate failed twice
  APP-5  APP-5 title
  APP-4  APP-4 title
  APP-3  APP-3 title
  +2 more

Outside any goal: 3 landed (WEB 2 · APP 1)
  WEB-4  WEB-4 title
  WEB-3  WEB-3 title
  APP-20  APP-20 title

You were needed 4 times on 2 issues: 3 questions, 1 approval · 2 still wait
   9:05 PM  app-8  2 questions, 1 approval · 2 wait: the change
   9:01 PM  APP-7  asked: Which rounding?

Triage answered a question without you
";
        assert_eq!(text, expected);
        // Everything, unfolded.
        assert!(text_all_has(&report, &tz, "APP-1  APP-1 title"));
    }

    fn text_all_has(r: &Report, tz: &FixedOffset, line: &str) -> bool {
        text(r, tz, true).contains(line)
    }

    #[test]
    fn a_quiet_night_says_so() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let r = Report { since: 3600, now: 7200, goals: vec![], other_landed: vec![], needed: vec![], triaged: vec![] };
        assert_eq!(text(&r, &tz, false), "Since 1:00 AM (1 hour)\n\nNo active goals.\n\nYou were not needed.\n");
    }

    #[test]
    fn a_long_line_is_cut_on_one_line() {
        assert_eq!(short("a\nb  c"), "a b c");
        let cut = short(&"word ".repeat(40));
        assert!(cut.ends_with('…') && cut.chars().count() <= WIDTH, "{cut}");
    }
}
