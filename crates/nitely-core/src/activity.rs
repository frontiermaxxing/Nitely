//! The Activity view's data: what every agent session did over a window of time. One lane per
//! session, with its state over time (working, waiting for the human, idle, quiet) and marks for
//! what happened on it: an issue started, a commit, a gate, a review verdict, a land, an ask.
//!
//! States come from `session_state`, which `Db::record` fills from the hooks. Before its first row
//! nothing is known about states, and `recorded_since` says so rather than guessing. Issue events
//! are written by the MCP server under one id per agent and folder (`claude:c:/development/nitely`),
//! which every session of that agent in that folder shares; such an event goes to the session that
//! named its issue in a tool call nearest in time.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::db::{now, Db, IssueRecord, Session};
use crate::paths::{is_within, normalize};

/// Working or idle this long with nothing recorded reads as stale, as on the Now view.
pub const STALE_SECS: i64 = 30 * 60;
/// How close to a session's own activity an event must be to go to it without an issue to match.
const NEAR_SECS: i64 = 120;
/// How far outside a session's known lifetime an agent id's state or event may still go to it,
/// when it is the only session of that agent in that folder near then. Codex's hook session first
/// appears when its first turn ends, after the turn's work was reported.
const LIFETIME_SLACK_SECS: i64 = 3 * 3600;
/// Before states were recorded, hook calls this close together count as one stretch of activity.
const SEEN_GAP_SECS: i64 = 10 * 60;
/// A lane shorter than this with no marks (a session opened and closed at once) is left out.
const MIN_LANE_SECS: i64 = 120;

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub from: i64,
    pub to: i64,
    /// When session states started being recorded; None while nothing is. Before it, lanes have
    /// marks but no states.
    pub recorded_since: Option<i64>,
    pub lanes: Vec<Lane>,
    /// Every issue a lane or mark names, so the view can draw each as an issue token.
    pub issues: Vec<IssueRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lane {
    pub session_id: String,
    pub vendor: String,
    pub cwd: String,
    /// The project of the repo it ran in, else of the issue it touched last.
    pub project: Option<String>,
    /// The folder it ran in, by name.
    pub place: String,
    /// Issues it worked on in the window, the most recent last.
    pub issues: Vec<String>,
    pub started_at: i64,
    pub last_seen_at: i64,
    /// Its state when last seen: working, waiting, idle or ended.
    pub state: String,
    pub segments: Vec<Segment>,
    pub marks: Vec<Mark>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Segment {
    pub from: i64,
    pub to: i64,
    /// working, waiting (for the human), idle, stale (nothing recorded for `STALE_SECS`), or seen:
    /// active before states were recorded, so its state is not known.
    pub state: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Mark {
    pub ts: i64,
    /// start, commit, gate_pass, gate_fail, submit, review_pass, review_changes, review_fail,
    /// land, land_fail, ask or handoff.
    pub kind: String,
    pub issue_key: Option<String>,
    pub summary: String,
}

struct Event {
    ts: i64,
    actor: String,
    kind: String,
    summary: String,
    issue: Option<String>,
}

struct LaneBuild {
    session: Session,
    /// (ts, issue key) for each issue its own tool calls named.
    touches: Vec<(i64, String)>,
    /// When it did anything: its own events and the ones it was given.
    activity: Vec<i64>,
    rows: Vec<(i64, String)>,
    /// States written under the agent id this session was folded with.
    folded_rows: Vec<(i64, String)>,
    marks: Vec<Mark>,
}

impl LaneBuild {
    fn new(session: &Session) -> LaneBuild {
        LaneBuild { session: session.clone(), touches: vec![], activity: vec![], rows: vec![], folded_rows: vec![], marks: vec![] }
    }

    /// Seconds from `ts` to the session's lifetime; 0 inside it.
    fn distance(&self, ts: i64) -> i64 {
        (self.session.started_at - ts).max(ts - self.session.last_seen_at).max(0)
    }
}

/// Of `candidates`, the one session whose lifetime is nearest `ts`, when it is the only one that
/// near and within `LIFETIME_SLACK_SECS`; with two equally near there is no telling which.
fn nearest_lifetime(lanes: &[LaneBuild], candidates: &[usize], ts: i64) -> Option<usize> {
    let best = candidates.iter().map(|&i| lanes[i].distance(ts)).min()?;
    let at_best: Vec<usize> = candidates.iter().copied().filter(|&i| lanes[i].distance(ts) == best).collect();
    match at_best[..] {
        [i] if best <= LIFETIME_SLACK_SECS => Some(i),
        _ => None,
    }
}

/// An id the MCP server or the CLI wrote under (`claude:c:/development/nitely`, `cli:...`), not a
/// hook's session id.
fn is_agent_id(s: &Session) -> bool {
    s.vendor == "cli" || s.id == format!("{}:{}", s.vendor, normalize(&s.cwd))
}

pub fn activity(config: &Config, db: &Db, from: i64, to: i64) -> Result<Activity> {
    let to = to.min(now());
    let conn = &db.conn;
    let recorded_since: Option<i64> = conn.query_row("SELECT MIN(ts) FROM session_state", [], |r| r.get(0))?;
    let sessions = sessions_in(db, from, to)?;
    let keys: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT key FROM issue")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let prefixes: HashSet<String> = keys.iter().filter_map(|k| k.split_once('-')).map(|(p, _)| p.to_lowercase()).collect();
    let events: Vec<Event> = {
        let mut stmt = conn.prepare(
            "SELECT ts, actor, type, COALESCE(summary, ''), issue_key FROM event WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts, id",
        )?;
        let rows = stmt.query_map(params![from, to], |r| {
            Ok(Event { ts: r.get(0)?, actor: r.get(1)?, kind: r.get(2)?, summary: r.get(3)?, issue: r.get(4)? })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    // Hook sessions are the lanes. An agent id is folded into the hook sessions of its vendor in
    // its folder, and is a lane of its own only when there are none (an agent without hooks).
    let same_place = |a: &Session, b: &Session| a.vendor == b.vendor && normalize(&a.cwd) == normalize(&b.cwd);
    let mut lanes: Vec<LaneBuild> = Vec::new();
    for s in sessions.iter().filter(|s| !is_agent_id(s)) {
        lanes.push(LaneBuild::new(s));
    }
    let folded: Vec<&Session> = sessions
        .iter()
        .filter(|s| is_agent_id(s))
        .filter(|s| {
            let has_hooks = lanes.iter().any(|l| same_place(&l.session, s));
            if !has_hooks {
                lanes.push(LaneBuild::new(s));
            }
            has_hooks
        })
        .collect();
    let index: HashMap<String, usize> = lanes.iter().enumerate().map(|(i, l)| (l.session.id.clone(), i)).collect();

    // What each lane did itself.
    for e in &events {
        if let Some(&i) = index.get(&e.actor) {
            lanes[i].activity.push(e.ts);
            for k in issue_keys(&e.summary, &prefixes, &keys).into_iter().chain(e.issue.clone()) {
                lanes[i].touches.push((e.ts, k));
            }
        }
    }
    for lane in &mut lanes {
        let before: Option<i64> =
            conn.query_row("SELECT MAX(ts) FROM event WHERE actor = ?1 AND ts < ?2", params![lane.session.id, from], |r| r.get(0))?;
        lane.activity.extend(before);
        lane.rows = state_rows(db, &lane.session.id, from, to)?;
    }

    // An agent id's states go to the one hook session of that agent and folder nearest the time;
    // with two equally near (both running then) there is no telling which one changed.
    for s in &folded {
        let place: Vec<usize> = (0..lanes.len()).filter(|&i| same_place(&lanes[i].session, s)).collect();
        for (ts, state) in state_rows(db, &s.id, from, to)? {
            if let Some(i) = nearest_lifetime(&lanes, &place, ts) {
                lanes[i].folded_rows.push((ts, state));
            }
        }
    }

    // Marks: a lane's own commits, and issue events wherever they belong.
    for e in &events {
        let Some(kind) = mark_kind(e) else { continue };
        let Some(i) = index.get(&e.actor).copied().or_else(|| attribute(&lanes, e)) else { continue };
        let issue = e.issue.clone().or_else(|| issue_keys(&e.summary, &prefixes, &keys).into_iter().next()).or_else(|| {
            lanes[i].touches.iter().filter(|(t, _)| *t <= e.ts).max_by_key(|(t, _)| *t).map(|(_, k)| k.clone())
        });
        let lane = &mut lanes[i];
        // claimed and worktree both mean the work started; keep one.
        if kind == "start" && lane.marks.iter().any(|m| m.kind == "start" && m.issue_key == issue && e.ts - m.ts < 600) {
            continue;
        }
        if index.get(&e.actor) != Some(&i) {
            lane.activity.push(e.ts);
        }
        let summary = if kind == "commit" { "git commit".to_string() } else { e.summary.clone() };
        lane.marks.push(Mark { ts: e.ts, kind: kind.into(), issue_key: issue, summary });
    }

    let mut out: Vec<Lane> = lanes
        .into_iter()
        .filter_map(|mut l| {
            // The hook's own state wins a tie: it is the finer record of the two.
            let mut rows = std::mem::take(&mut l.folded_rows);
            rows.append(&mut l.rows);
            rows.sort_by_key(|(ts, _)| *ts);
            l.activity.sort_unstable();
            let recorded_from = recorded_since.unwrap_or(to);
            let mut segments = seen(&l.activity, from, recorded_from.min(to));
            segments.extend(segments_of(&rows, &l.activity, from, to));
            // A lane that is only stale (a session left idle days ago) says nothing about the window.
            let length: i64 = segments.iter().filter(|s| s.state != "stale").map(|s| s.to - s.from).sum();
            if l.marks.is_empty() && length < MIN_LANE_SECS {
                return None;
            }
            let mut issues: Vec<(i64, String)> = Vec::new();
            for (ts, k) in l.touches.iter().filter(|(t, _)| *t >= from).cloned().chain(l.marks.iter().filter_map(|m| Some((m.ts, m.issue_key.clone()?)))) {
                match issues.iter_mut().find(|(_, x)| *x == k) {
                    Some(entry) => entry.0 = entry.0.max(ts),
                    None => issues.push((ts, k)),
                }
            }
            issues.sort_by_key(|(t, _)| *t);
            let cwd = l.session.cwd.clone();
            let project = config
                .repos
                .iter()
                .find(|r| is_within(&cwd, &r.root))
                .and_then(|r| r.project.clone())
                .or_else(|| issues.last().and_then(|(_, k)| k.split_once('-')).map(|(p, _)| p.to_string()));
            let place = cwd.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
            Some(Lane {
                session_id: l.session.id,
                vendor: l.session.vendor,
                cwd,
                project,
                place,
                issues: issues.into_iter().map(|(_, k)| k).collect(),
                started_at: l.session.started_at,
                last_seen_at: l.session.last_seen_at,
                state: l.session.state,
                segments,
                marks: l.marks,
            })
        })
        .collect();
    let rank = |s: &str| match s {
        "waiting" => 0,
        "working" => 1,
        "idle" => 2,
        _ => 3,
    };
    out.sort_by(|a, b| {
        (a.project.is_none(), &a.project, rank(&a.state), std::cmp::Reverse(a.last_seen_at)).cmp(&(
            b.project.is_none(),
            &b.project,
            rank(&b.state),
            std::cmp::Reverse(b.last_seen_at),
        ))
    });
    let mut named: Vec<String> = Vec::new();
    for k in out.iter().flat_map(|l| l.issues.iter().chain(l.marks.iter().filter_map(|m| m.issue_key.as_ref()))) {
        if !named.contains(k) {
            named.push(k.clone());
        }
    }
    let issues = named.iter().filter_map(|k| db.issue(k).transpose()).collect::<Result<Vec<_>>>()?;
    Ok(Activity { from, to, recorded_since, lanes: out, issues })
}

/// A session's state changes in the window, starting with the one in force at `from`.
fn state_rows(db: &Db, session_id: &str, from: i64, to: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = db.conn.prepare(
        "SELECT ts, state FROM session_state
         WHERE session_id = ?1 AND ts <= ?3
           AND ts >= COALESCE((SELECT MAX(ts) FROM session_state WHERE session_id = ?1 AND ts <= ?2), ?2)
         ORDER BY ts, rowid",
    )?;
    let rows = stmt.query_map(params![session_id, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The lane an issue event written under an agent id (or by the CLI) belongs to: of the sessions
/// that could have written it, the one that named its issue nearest in time, else the only one
/// active around then.
fn attribute(lanes: &[LaneBuild], e: &Event) -> Option<usize> {
    let (vendor, place) = e.actor.split_once(':').unwrap_or((e.actor.as_str(), ""));
    if vendor == "human" {
        return None;
    }
    let of_vendor = |l: &LaneBuild| vendor == "cli" || l.session.vendor == vendor;
    let in_place: Vec<usize> = (0..lanes.len()).filter(|&i| of_vendor(&lanes[i]) && normalize(&lanes[i].session.cwd) == place).collect();
    let candidates: Vec<usize> = if in_place.is_empty() { (0..lanes.len()).filter(|&i| of_vendor(&lanes[i])).collect() } else { in_place };
    if let Some(key) = &e.issue {
        let nearest = candidates
            .iter()
            .filter_map(|&i| lanes[i].touches.iter().filter(|(_, k)| k == key).map(|(t, _)| (i, (t - e.ts).abs())).min_by_key(|x| x.1))
            .min_by_key(|x| x.1);
        if let Some((i, _)) = nearest {
            return Some(i);
        }
    }
    let near: Vec<usize> = candidates.iter().copied().filter(|&i| lanes[i].activity.iter().any(|t| (t - e.ts).abs() <= NEAR_SECS)).collect();
    match near[..] {
        [i] => Some(i),
        [] => nearest_lifetime(lanes, &candidates, e.ts),
        _ => None,
    }
}

/// Sessions that may have done something in `from..to`: seen in it, or not ended by then (one still
/// waiting on the human sends nothing while it waits).
fn sessions_in(db: &Db, from: i64, to: i64) -> Result<Vec<Session>> {
    let mut stmt = db.conn.prepare(
        "SELECT id, vendor, cwd, started_at, last_seen_at, last_activity, state FROM session
         WHERE started_at <= ?2 AND (last_seen_at >= ?1 OR state != 'ended')",
    )?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok(Session {
            id: r.get(0)?,
            vendor: r.get(1)?,
            cwd: r.get(2)?,
            started_at: r.get(3)?,
            last_seen_at: r.get(4)?,
            last_activity: r.get(5)?,
            state: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn mark_kind(e: &Event) -> Option<&'static str> {
    Some(match e.kind.as_str() {
        "claimed" | "worktree" => "start",
        "gate_passed" => "gate_pass",
        "gate_failed" | "setup_failed" => "gate_fail",
        "submit" if e.summary.contains("requested changes") => "review_changes",
        "submit" if e.summary.contains("passed") => "review_pass",
        "submit" => "submit",
        "review_failed" => "review_fail",
        "landed" => "land",
        "land_failed" => "land_fail",
        "ask" => "ask",
        "handoff" => "handoff",
        "tool" if is_commit(&e.summary) => "commit",
        _ => return None,
    })
}

/// A shell command that commits (`git commit`, also inside `cd x && git commit ...`).
fn is_commit(summary: &str) -> bool {
    let (tool, command) = summary.split_once(": ").unwrap_or(("", ""));
    matches!(tool, "Bash" | "PowerShell" | "Shell")
        && command.split(['&', ';', '|']).any(|part| {
            let words: Vec<&str> = part.split_whitespace().collect();
            words.windows(2).any(|w| w == ["git", "commit"]) && !words.contains(&"--dry-run")
        })
}

/// Issue keys named in a summary: `NIT-27`, or a worktree folder like `nit-27-activity-lanes`.
/// Only keys of stored issues count, so `utf-8` is not an issue.
fn issue_keys(summary: &str, prefixes: &HashSet<String>, keys: &HashSet<String>) -> Vec<String> {
    let lower = summary.to_lowercase();
    let bytes = lower.as_bytes();
    let mut out: Vec<String> = Vec::new();
    for (i, _) in lower.char_indices() {
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        for p in prefixes {
            let Some(rest) = lower[i..].strip_prefix(p.as_str()).and_then(|r| r.strip_prefix('-')) else { continue };
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() || rest[digits.len()..].starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            let key = format!("{}-{digits}", p.to_uppercase());
            if keys.contains(&key) && !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

/// A lane's states as segments within `from..to`. Nothing follows an end until a new state.
/// Working or idle with no activity for `STALE_SECS` turns stale until the next activity; waiting
/// stays waiting, since that is the human's turn however long it takes.
fn segments_of(rows: &[(i64, String)], activity: &[i64], from: i64, to: i64) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut push = |a: i64, b: i64, state: &str| {
        let (a, b) = (a.max(from), b.min(to));
        if b <= a {
            return;
        }
        match out.last_mut() {
            Some(last) if last.state == state && last.to == a => last.to = b,
            _ => out.push(Segment { from: a, to: b, state: state.into() }),
        }
    };
    for (n, (start, state)) in rows.iter().enumerate() {
        let end = rows.get(n + 1).map_or(to, |r| r.0).min(to);
        if state == "ended" || end <= *start {
            continue;
        }
        if state == "waiting" {
            push(*start, end, state);
            continue;
        }
        // A state change comes from a hook call, so it is activity itself.
        let mut last = *start;
        let mut cursor = *start;
        for t in activity.iter().copied().filter(|&t| t > *start && t < end).chain([end]) {
            if t - last > STALE_SECS {
                let quiet = (last + STALE_SECS).max(cursor);
                push(cursor, quiet, state);
                push(quiet, t, "stale");
                cursor = t;
            }
            last = t;
        }
        push(cursor, end, state);
    }
    out
}

/// Stretches of hook activity in `from..to`, for the time before states were recorded: the
/// session was doing something, but whether it worked, waited or idled is not known.
fn seen(activity: &[i64], from: i64, to: i64) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for &t in activity.iter().filter(|&&t| t >= from && t < to) {
        match out.last_mut() {
            Some(last) if t - last.to <= SEEN_GAP_SECS => last.to = (t + 60).min(to),
            _ => out.push(Segment { from: t, to: (t + 60).min(to), state: "seen".into() }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::{Actor, NewIssue};

    fn db() -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "nitely-activity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        (Db::open(&dir.join("t.db")).unwrap(), dir)
    }

    fn session(db: &Db, id: &str, cwd: &str, started: i64, seen: i64, state: &str) {
        db.conn
            .execute(
                "INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, id.split(':').next().unwrap(), cwd, started, seen, state],
            )
            .unwrap();
    }

    fn state(db: &Db, id: &str, ts: i64, state: &str) {
        db.conn.execute("INSERT INTO session_state (session_id, ts, state) VALUES (?1, ?2, ?3)", params![id, ts, state]).unwrap();
    }

    fn event(db: &Db, ts: i64, actor: &str, kind: &str, summary: &str, issue: Option<&str>) {
        db.conn
            .execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, actor, issue, kind, summary],
            )
            .unwrap();
    }

    #[test]
    fn lanes_follow_states_and_go_quiet() {
        let rows: Vec<(i64, String)> = [(1000, "working"), (5000, "idle"), (9000, "ended")].iter().map(|(t, s)| (*t, s.to_string())).collect();
        // Busy until 2000, then nothing until 4500: stale from 3800.
        let segs = segments_of(&rows, &[1000, 1500, 2000, 4500], 0, 20000);
        let got: Vec<(i64, i64, &str)> = segs.iter().map(|s| (s.from, s.to, s.state.as_str())).collect();
        assert_eq!(got, [(1000, 3800, "working"), (3800, 4500, "stale"), (4500, 5000, "working"), (5000, 6800, "idle"), (6800, 9000, "stale")]);
        // Clipped to the window; waiting never goes stale.
        let rows: Vec<(i64, String)> = vec![(0, "waiting".into())];
        assert_eq!(segments_of(&rows, &[], 100, 9000), vec![Segment { from: 100, to: 9000, state: "waiting".into() }]);
        // Before recording: activity close together is one stretch, a long pause splits it.
        let got: Vec<(i64, i64)> = seen(&[100, 300, 500, 2000, 9500], 0, 9000).iter().map(|s| (s.from, s.to)).collect();
        assert_eq!(got, [(100, 560), (2000, 2060)]);
    }

    #[test]
    fn finds_issue_keys_and_commits() {
        let keys: HashSet<String> = ["NIT-27", "NIT-2", "AND-5"].iter().map(|s| s.to_string()).collect();
        let prefixes: HashSet<String> = ["nit".to_string(), "and".to_string()].into();
        let k = |s: &str| issue_keys(s, &prefixes, &keys);
        assert_eq!(k("Bash: cd C:/Development/Nitely-worktrees/nit-27-activity && cargo test"), ["NIT-27"]);
        assert_eq!(k("mcp__nitely__submit: NIT-27"), ["NIT-27"]);
        assert!(k("Read: nit-270 and-x utf-8").is_empty());
        assert!(is_commit("Bash: cd x && git add -A && git commit -q -F - <<'EOF'"));
        assert!(!is_commit("Bash: git commit --dry-run") && !is_commit("Read: docs/git commit.md") && !is_commit("Bash: git log"));
    }

    #[test]
    fn issue_events_go_to_the_session_that_named_the_issue() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        for t in ["Lanes", "Pulse"] {
            db.create_issue(&NewIssue { project: "NIT".into(), title: t.into(), body: "b".into(), kind: "feature".into(), source: "claude".into(), evidence: "e".into(), ..Default::default() }, &h).unwrap();
        }
        let t0 = now() - 4 * 3600;
        let cwd = "C:\\Development\\Nitely";
        // Two Claude sessions in one folder, and the MCP id they share.
        session(&db, "claude:a", cwd, t0, t0 + 3000, "idle");
        session(&db, "claude:b", cwd, t0, t0 + 3000, "working");
        session(&db, "claude:c:/development/nitely", cwd, t0, t0 + 3000, "working");
        state(&db, "claude:a", t0, "working");
        state(&db, "claude:a", t0 + 2000, "idle");
        state(&db, "claude:b", t0, "working");
        event(&db, t0 + 100, "claude:a", "tool", "mcp__nitely__start_task: NIT-1", None);
        event(&db, t0 + 110, "claude:b", "tool", "mcp__nitely__start_task: NIT-2", None);
        event(&db, t0 + 105, "claude:c:/development/nitely", "worktree", "Started claude/nit-1-lanes", Some("NIT-1"));
        event(&db, t0 + 900, "claude:b", "tool", "Bash: cd C:/Development/Nitely-worktrees/nit-2-pulse && git commit -qm x", None);
        event(&db, t0 + 1500, "claude:c:/development/nitely", "gate_failed", "Gate failed", Some("NIT-2"));
        event(&db, t0 + 1600, "human", "answer", "yes", Some("NIT-1"));
        let config = Config::default();
        let a = activity(&config, &db, t0 - 600, now()).unwrap();
        assert_eq!(a.recorded_since, Some(t0));
        let ids: Vec<&str> = a.lanes.iter().map(|l| l.session_id.as_str()).collect();
        assert_eq!(ids, ["claude:b", "claude:a"], "the MCP id is folded into the hook sessions; working sorts first");
        let marks = |id: &str| -> Vec<(String, Option<String>)> {
            a.lanes.iter().find(|l| l.session_id == id).unwrap().marks.iter().map(|m| (m.kind.clone(), m.issue_key.clone())).collect()
        };
        assert_eq!(marks("claude:a"), [("start".to_string(), Some("NIT-1".to_string()))]);
        assert_eq!(marks("claude:b"), [("commit".to_string(), Some("NIT-2".to_string())), ("gate_fail".to_string(), Some("NIT-2".to_string()))]);
        assert_eq!(a.issues.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), ["NIT-2", "NIT-1"]);
        let b = &a.lanes[0];
        assert_eq!(b.issues, ["NIT-2"]);
        assert_eq!(b.segments.first().map(|s| (s.from, s.state.as_str())), Some((t0, "working")));
        assert!(b.segments.iter().any(|s| s.state == "stale"), "nothing since the gate");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn codex_work_before_its_first_turn_end_and_long_waits_are_kept() {
        let (db, dir) = db();
        let t0 = now() - 3 * 3600;
        let cwd = "C:\\Development\\Harbor";
        // Codex reports work over MCP for an hour; its hook session appears at the first turn end.
        session(&db, "codex:c:/development/harbor", cwd, t0, t0 + 3600, "working");
        session(&db, "codex:thread-1", cwd, t0 + 3600, t0 + 3600, "idle");
        state(&db, "codex:c:/development/harbor", t0, "working");
        state(&db, "codex:thread-1", t0 + 3600, "idle");
        event(&db, t0 + 600, "codex:c:/development/harbor", "gate_passed", "Gate passed", None);
        // A Claude session that asked something two hours ago and has waited since.
        session(&db, "claude:w", "C:\\Development\\Nitely", t0, t0 + 1200, "waiting");
        state(&db, "claude:w", t0, "working");
        state(&db, "claude:w", t0 + 1200, "waiting");
        let a = activity(&Config::default(), &db, now() - 3600, now()).unwrap();
        let w = a.lanes.iter().find(|l| l.session_id == "claude:w").expect("still waiting, so still shown");
        assert_eq!(w.segments.iter().map(|s| s.state.as_str()).collect::<Vec<_>>(), ["waiting"]);
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:thread-1").unwrap();
        assert_eq!(codex.segments.first().map(|s| (s.from, s.state.as_str())), Some((t0, "working")));
        assert_eq!(codex.marks.iter().map(|m| m.kind.as_str()).collect::<Vec<_>>(), ["gate_pass"]);
        // At the same second, the hook's idle beats the MCP id's working.
        state(&db, "codex:c:/development/harbor", t0 + 3600, "working");
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:thread-1").unwrap();
        assert!(codex.segments.iter().any(|s| s.from == t0 + 3600 && s.state == "idle"), "{:?}", codex.segments);
        std::fs::remove_dir_all(dir).ok();
    }
}
