//! The Activity view's data: what every agent session did over a window of time. One lane per
//! session, with its state over time (working, waiting for the human, idle, quiet) and marks for
//! what happened on it: an issue started, a commit, a gate, a review verdict, a land, an ask.
//!
//! States come from `session_state`, which `Db::record` fills from the hooks. Before its first row
//! nothing is known about states, and `recorded_since` says so rather than guessing. Issue events
//! are written by the MCP server under one id per agent and folder (`claude:c:/code/nitely`),
//! which every session of that agent in that folder shares; such an event goes to the session that
//! named its issue in a tool call nearest in time. Codex is the other way round: its hooks only say
//! when a turn ends, so its MCP id is the lane and its hook sessions in that folder are folded in.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
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
    /// Every session whose own events and states are this lane's: the session itself, and for
    /// Codex the hook sessions folded into its MCP id.
    members: Vec<Session>,
    /// (ts, issue key) for each issue its own tool calls named.
    touches: Vec<(i64, String)>,
    /// When it did anything: its own events and the ones it was given.
    activity: Vec<i64>,
    /// (ts, rank, state): at the same second a hook's state (rank 1) beats an MCP id's (rank 0),
    /// since the hook's is the finer record.
    rows: Vec<(i64, u8, String)>,
    marks: Vec<Mark>,
}

impl LaneBuild {
    fn new(session: &Session) -> LaneBuild {
        LaneBuild { session: session.clone(), members: vec![session.clone()], touches: vec![], activity: vec![], rows: vec![], marks: vec![] }
    }

    /// Seconds from `ts` to the session's lifetime; 0 inside it.
    fn distance(&self, ts: i64) -> i64 {
        self.members.iter().map(|m| (m.started_at - ts).max(ts - m.last_seen_at).max(0)).min().unwrap_or(i64::MAX)
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

/// Whether two sessions' lifetimes come within `LIFETIME_SLACK_SECS` of each other.
fn overlap(a: &Session, b: &Session) -> bool {
    a.started_at - LIFETIME_SLACK_SECS <= b.last_seen_at && b.started_at - LIFETIME_SLACK_SECS <= a.last_seen_at
}

/// An id the MCP server or the CLI wrote under (`claude:c:/code/nitely`, `cli:...`), not a
/// hook's session id. Its place is compared normalized, so an id written before short names
/// were spelled out (`claude:c:/progra~1/app`, NIT-104) still counts (NIT-134).
fn is_agent_id(s: &Session) -> bool {
    s.vendor == "cli" || s.id.split_once(':').is_some_and(|(vendor, place)| vendor == s.vendor && normalize(place) == normalize(&s.cwd))
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
    // its folder that ran around the same time, and is a lane of its own when there are none (an
    // agent without hooks). Codex's hooks only report turn ends, so for Codex the agent id is the
    // lane and those hook sessions are folded into it instead. A Codex that names its thread on
    // each tool call has its reports recorded under the thread's own session (mcp.rs,
    // `Server::session`), so it never has that agent id and each thread is a lane (NIT-36).
    let same_place = |a: &Session, b: &Session| a.vendor == b.vendor && normalize(&a.cwd) == normalize(&b.cwd);
    let mut lanes: Vec<LaneBuild> = Vec::new();
    let mut folded: Vec<&Session> = Vec::new();
    let codex_ids: Vec<&Session> = sessions.iter().filter(|s| is_agent_id(s) && s.vendor == "codex").collect();
    for s in sessions.iter().filter(|s| !is_agent_id(s)) {
        match lanes.iter_mut().find(|l| codex_ids.iter().any(|c| c.id == l.session.id) && same_place(&l.session, s) && overlap(&l.session, s)) {
            Some(lane) => lane.members.push(s.clone()),
            None => match codex_ids.iter().find(|c| s.vendor == "codex" && same_place(c, s) && overlap(c, s)) {
                Some(c) => {
                    let mut lane = LaneBuild::new(c);
                    lane.members.push(s.clone());
                    lanes.push(lane);
                }
                None => lanes.push(LaneBuild::new(s)),
            },
        }
    }
    for s in sessions.iter().filter(|s| is_agent_id(s)) {
        if lanes.iter().any(|l| l.session.id == s.id) {
            continue;
        }
        if lanes.iter().any(|l| same_place(&l.session, s) && overlap(&l.session, s)) {
            folded.push(s);
        } else {
            lanes.push(LaneBuild::new(s));
        }
    }
    let index: HashMap<String, usize> =
        lanes.iter().enumerate().flat_map(|(i, l)| l.members.iter().map(move |m| (m.id.clone(), i))).collect();

    // What each lane did itself.
    for e in &events {
        if let Some(&i) = index.get(&e.actor) {
            lanes[i].activity.push(e.ts);
            for k in issue_keys(&e.summary, &prefixes, &keys).into_iter().chain(e.issue.clone()) {
                lanes[i].touches.push((e.ts, k));
            }
        }
    }
    let mut by_actor: HashMap<&str, Vec<i64>> = HashMap::new();
    for e in &events {
        by_actor.entry(e.actor.as_str()).or_default().push(e.ts);
    }
    // Each id's last event before the window: what it did last, and where its state was last said.
    let last_before = |id: &str| -> Result<Option<i64>> {
        Ok(conn.query_row("SELECT MAX(ts) FROM event WHERE actor = ?1 AND ts < ?2", params![id, from], |r| r.get(0))?)
    };
    // Each id's state rows only mark changes of its own. Where a lane merges two ids (Codex's MCP
    // id and its turn-end hook), each event must say its id's state again, or a report after a
    // turn end, or a turn end after a report, would change nothing. That holds for the last event
    // before the window too, or a report just before `from` would be lost to an earlier turn end
    // (NIT-43).
    let states_of = |id: &str, before: Option<i64>| -> Result<Vec<(i64, String)>> {
        let events: Vec<i64> = before.into_iter().chain(by_actor.get(id).into_iter().flatten().copied()).collect();
        Ok(reasserted(state_rows(db, id, from, to)?, &events))
    };
    for lane in &mut lanes {
        for m in lane.members.clone() {
            let before = last_before(&m.id)?;
            lane.activity.extend(before);
            let rank = u8::from(!is_agent_id(&m));
            lane.rows.extend(states_of(&m.id, before)?.into_iter().map(|(ts, st)| (ts, rank, st)));
        }
    }

    // A folded agent id's states go to the one hook session of that agent and folder nearest the
    // time; with two equally near (both running then) there is no telling which one changed.
    for s in &folded {
        let place: Vec<usize> = (0..lanes.len()).filter(|&i| same_place(&lanes[i].session, s) && overlap(&lanes[i].session, s)).collect();
        for (ts, state) in states_of(&s.id, last_before(&s.id)?)? {
            if let Some(i) = nearest_lifetime(&lanes, &place, ts) {
                lanes[i].rows.push((ts, 0, state));
            }
        }
    }

    // Waiting on an ask lasts only while the ask is open: from its answer on, or from the start
    // when it is gone (its project was removed), the lane reads idle until it says otherwise.
    for lane in &mut lanes {
        let ids: Vec<String> = lane.members.iter().map(|m| m.id.clone()).collect();
        let ends = ask_waits_end(db, &ids, &lane.rows)?;
        lane.rows.extend(ends.into_iter().map(|ts| (ts, 2, "idle".to_string())));
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
            l.rows.sort_by_key(|(ts, rank, _)| (*ts, *rank));
            let rows: Vec<(i64, String)> = l.rows.iter().map(|(ts, _, st)| (*ts, st.clone())).collect();
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
            let latest = l.members.iter().max_by_key(|m| m.last_seen_at).unwrap_or(&l.session).clone();
            let started_at = l.members.iter().map(|m| m.started_at).min().unwrap_or(l.session.started_at);
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
                started_at,
                last_seen_at: latest.last_seen_at,
                // Waiting no longer, when the ask it waited on closed and nothing came after.
                state: match segments.last() {
                    Some(s) if latest.state == "waiting" && s.state != "waiting" => "idle".to_string(),
                    _ => latest.state,
                },
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

/// When each wait in `rows` that an ask began stops being a wait: the ask's answer, or the wait's
/// own start when the ask no longer exists. An ask records its wait at the moment it is asked, as
/// an `ask` event of one of `ids` saying "Asked: <question>", from the folder the ask was asked in.
/// A wait still open, one a Notification hook began (a permission prompt), and one the session
/// moved on from before the answer came (it worked, went idle, asked again or ended) stay as they
/// are; the same wait said again (a note written while waiting) is not moving on.
fn ask_waits_end(db: &Db, ids: &[String], rows: &[(i64, u8, String)]) -> Result<Vec<i64>> {
    // The waits an ask began: when, the question, and the folder it was asked from.
    let mut asked: Vec<(i64, String, Option<String>)> = Vec::new();
    for &(ts, _, ref state) in rows {
        if state != "waiting" || asked.iter().any(|(t, _, _)| *t == ts) {
            continue;
        }
        for id in ids {
            let found: Option<(String, Option<String>)> = db
                .conn
                .query_row(
                    "SELECT summary, cwd FROM event WHERE actor = ?1 AND type = 'ask' AND ts = ?2 AND summary LIKE 'Asked: %' ORDER BY id DESC LIMIT 1",
                    params![id, ts],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((summary, cwd)) = found {
                asked.push((ts, summary.trim_start_matches("Asked: ").to_string(), cwd));
                break;
            }
        }
    }
    let mut out = Vec::new();
    for (ts, question, cwd) in &asked {
        let place = cwd.as_deref().map(normalize);
        let candidates: Vec<(Option<String>, Option<i64>)> = {
            let mut stmt = db.conn.prepare("SELECT cwd, answered_at FROM ask WHERE question = ?1 AND asked_at BETWEEN ?2 - 5 AND ?2 + 5 ORDER BY ABS(asked_at - ?2)")?;
            let found = stmt.query_map(params![question, ts], |r| Ok((r.get(0)?, r.get(1)?)))?;
            found.collect::<rusqlite::Result<_>>()?
        };
        // The same question from the same folder: another agent asking it elsewhere is not this
        // one. Where either side does not say its folder, the question and the moment decide.
        let same_place = |asked_in: &Option<String>| match (asked_in.as_deref().map(normalize), &place) {
            (Some(a), Some(p)) => a == *p,
            _ => true,
        };
        let ask = candidates.into_iter().find(|(asked_in, _)| same_place(asked_in)).map(|(_, answered)| answered);
        // What it did next on its own: another state, or a wait another ask began.
        let next = rows.iter().filter(|(t, _, st)| *t > *ts && (st != "waiting" || asked.iter().any(|(a, _, _)| a == t))).map(|(t, _, _)| *t).min();
        match ask {
            Some(Some(answered)) if next.is_none_or(|n| answered < n) => out.push(answered.max(*ts)),
            Some(_) => {}
            None => out.push(*ts),
        }
    }
    Ok(out)
}

/// `rows` plus the state in force at each of `events`: every event of a session says its state
/// again, which matters once its rows are merged with another session's.
fn reasserted(rows: Vec<(i64, String)>, events: &[i64]) -> Vec<(i64, String)> {
    let mut out = rows.clone();
    for &e in events {
        if let Some((_, state)) = rows.iter().rev().find(|(ts, _)| *ts <= e) {
            out.push((e, state.clone()));
        }
    }
    out.sort_by_key(|(ts, _)| *ts);
    out
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
    // An actor written before short names were spelled out still names its folder (NIT-134).
    let place = normalize(place);
    let of_vendor = |l: &LaneBuild| vendor == "cli" || l.session.vendor == vendor;
    let in_place: Vec<usize> = (0..lanes.len())
        .filter(|&i| of_vendor(&lanes[i]) && lanes[i].members.iter().any(|m| normalize(&m.cwd) == place))
        .collect();
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
            crew: false,
        })
    })?;
    let mut sessions: Vec<Session> = rows.collect::<rusqlite::Result<_>>()?;
    db.mark_crew(&mut sessions);
    Ok(sessions)
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
pub(crate) fn issue_keys(summary: &str, prefixes: &HashSet<String>, keys: &HashSet<String>) -> Vec<String> {
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
        let dir = crate::paths::unique_temp("nitely-activity", "");
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
    fn waiting_on_an_ask_lasts_while_the_ask_is_open() {
        let (db, dir) = db();
        let config = Config::default();
        let t0 = now() - 7200;
        let agent = crate::Actor::Agent("codex:x".into());
        // Three Codex threads ask and go quiet: one is answered, one's ask is gone (its project
        // was removed), one is still waiting on the person.
        for (id, question) in [("codex:answered", "Which branch?"), ("codex:gone", "May I write here?"), ("codex:open", "Ship it?")] {
            session(&db, id, "C:\\code\\app", t0, t0 + 60, "waiting");
            state(&db, id, t0, "working");
            state(&db, id, t0 + 60, "waiting");
            event(&db, t0 + 60, id, "ask", &format!("Asked: {question}"), None);
            db.conn.execute("UPDATE event SET cwd = ?2 WHERE actor = ?1 AND type = 'ask'", params![id, "C:/code/app"]).unwrap();
            let ask = db.create_ask(question, &[], None, Some("C:\\code\\app"), &agent).unwrap();
            db.conn.execute("UPDATE ask SET asked_at = ?2 WHERE id = ?1", params![ask.id, t0 + 61]).unwrap();
            match id {
                "codex:answered" => {
                    db.answer_ask(ask.id, "main", &crate::Actor::Human).unwrap();
                    db.conn.execute("UPDATE ask SET answered_at = ?2 WHERE id = ?1", params![ask.id, t0 + 600]).unwrap();
                }
                "codex:gone" => {
                    db.conn.execute("DELETE FROM ask WHERE id = ?1", [ask.id]).unwrap();
                }
                _ => {}
            }
        }
        // A permission prompt (Notification hook) is waiting with no ask, and stays so.
        session(&db, "claude:prompt", "C:\\code\\app", t0, t0 + 60, "waiting");
        state(&db, "claude:prompt", t0 + 60, "waiting");
        event(&db, t0 + 60, "claude:prompt", "waiting", "Waiting for input", None);

        // One writes a note while it waits (the same wait said again), and its ask, filed on an
        // issue, also left an issue event of the same moment with no folder: answered, it goes idle.
        session(&db, "codex:noted", "C:\\code\\app", t0, t0 + 200, "waiting");
        state(&db, "codex:noted", t0 + 60, "waiting");
        state(&db, "codex:noted", t0 + 200, "waiting");
        event(&db, t0 + 60, "codex:noted", "ask", "Which port?", None);
        event(&db, t0 + 60, "codex:noted", "ask", "Asked: Which port?", None);
        db.conn.execute("UPDATE event SET cwd = 'C:/code/app' WHERE actor = 'codex:noted' AND summary LIKE 'Asked: %'", []).unwrap();
        let noted = db.create_ask("Which port?", &[], None, Some("C:\\code\\app"), &agent).unwrap();
        db.answer_ask(noted.id, "5182", &crate::Actor::Human).unwrap();
        db.conn.execute("UPDATE ask SET asked_at = ?2, answered_at = ?3 WHERE id = ?1", params![noted.id, t0 + 60, t0 + 400]).unwrap();

        // One moved on before its answer came: the late answer changes nothing.
        session(&db, "codex:moved", "C:\\code\\app", t0, t0 + 300, "working");
        state(&db, "codex:moved", t0 + 60, "waiting");
        state(&db, "codex:moved", t0 + 300, "working");
        event(&db, t0 + 60, "codex:moved", "ask", "Asked: Rebase now?", None);
        db.conn.execute("UPDATE event SET cwd = 'C:/code/app' WHERE actor = 'codex:moved' AND type = 'ask'", []).unwrap();
        let late = db.create_ask("Rebase now?", &[], None, Some("C:\\code\\app"), &agent).unwrap();
        db.answer_ask(late.id, "yes", &crate::Actor::Human).unwrap();
        db.conn.execute("UPDATE ask SET asked_at = ?2, answered_at = ?3 WHERE id = ?1", params![late.id, t0 + 61, t0 + 900]).unwrap();
        // The same question asked from another folder at the same moment, and answered there.
        let elsewhere = db.create_ask("Ship it?", &[], None, Some("C:\\code\\other"), &agent).unwrap();
        db.answer_ask(elsewhere.id, "no", &crate::Actor::Human).unwrap();
        db.conn.execute("UPDATE ask SET asked_at = ?2, answered_at = ?3 WHERE id = ?1", params![elsewhere.id, t0 + 60, t0 + 120]).unwrap();

        let a = activity(&config, &db, t0 - 600, now()).unwrap();
        let lane = |id: &str| a.lanes.iter().find(|l| l.session_id == id).unwrap_or_else(|| panic!("{id}"));
        let waits = |id: &str| lane(id).segments.iter().filter(|s| s.state == "waiting").map(|s| (s.from - t0, s.to - t0)).collect::<Vec<_>>();
        assert_eq!(waits("codex:moved"), [(60, 300)], "until it moved on, not until the late answer");
        assert_eq!(waits("codex:noted").last().map(|w| w.1), Some(400), "until the answer, past the note");
        assert_eq!(lane("codex:moved").segments.iter().find(|s| s.from - t0 == 300).map(|s| s.state.as_str()), Some("working"));
        assert_eq!(waits("codex:answered"), [(60, 600)], "until the answer");
        assert!(waits("codex:gone").is_empty(), "an ask that is gone waits on no one");
        assert_eq!(lane("codex:open").segments.last().unwrap().state, "waiting");
        assert_eq!(lane("claude:prompt").segments.last().unwrap().state, "waiting");
        assert_eq!((lane("codex:answered").state.as_str(), lane("codex:gone").state.as_str(), lane("codex:open").state.as_str()), ("idle", "idle", "waiting"));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn finds_issue_keys_and_commits() {
        let keys: HashSet<String> = ["NIT-27", "NIT-2", "AND-5"].iter().map(|s| s.to_string()).collect();
        let prefixes: HashSet<String> = ["nit".to_string(), "and".to_string()].into();
        let k = |s: &str| issue_keys(s, &prefixes, &keys);
        assert_eq!(k("Bash: cd C:/Code/Nitely-worktrees/nit-27-activity && cargo test"), ["NIT-27"]);
        assert_eq!(k("mcp__nitely__submit: NIT-27"), ["NIT-27"]);
        assert!(k("Read: nit-270 and-x utf-8").is_empty());
        assert!(is_commit("Bash: cd x && git add -A && git commit -q -F - <<'EOF'"));
        assert!(!is_commit("Bash: git commit --dry-run") && !is_commit("Read: docs/git commit.md") && !is_commit("Bash: git log"));
    }

    // Folders are written the way `paths::normalize` leaves them on every platform (lowercase,
    // forward slashes), so an MCP id like `claude:c:/code/nitely` matches on Linux too.
    #[test]
    fn issue_events_go_to_the_session_that_named_the_issue() {
        let (mut db, dir) = db();
        db.create_project("NIT", "Nitely").unwrap();
        let h = Actor::Human;
        for t in ["Lanes", "Pulse"] {
            db.create_issue(&NewIssue { project: "NIT".into(), title: t.into(), body: "b".into(), kind: "feature".into(), source: "claude".into(), evidence: "e".into(), ..Default::default() }, &h).unwrap();
        }
        let t0 = now() - 4 * 3600;
        let cwd = "c:/code/nitely";
        // Two Claude sessions in one folder, and the MCP id they share.
        session(&db, "claude:a", cwd, t0, t0 + 3000, "idle");
        session(&db, "claude:b", cwd, t0, t0 + 3000, "working");
        session(&db, "claude:c:/code/nitely", cwd, t0, t0 + 3000, "working");
        state(&db, "claude:a", t0, "working");
        state(&db, "claude:a", t0 + 2000, "idle");
        state(&db, "claude:b", t0, "working");
        event(&db, t0 + 100, "claude:a", "tool", "mcp__nitely__start_task: NIT-1", None);
        event(&db, t0 + 110, "claude:b", "tool", "mcp__nitely__start_task: NIT-2", None);
        event(&db, t0 + 105, "claude:c:/code/nitely", "worktree", "Started claude/nit-1-lanes", Some("NIT-1"));
        event(&db, t0 + 900, "claude:b", "tool", "Bash: cd C:/Code/Nitely-worktrees/nit-2-pulse && git commit -qm x", None);
        event(&db, t0 + 1500, "claude:c:/code/nitely", "gate_failed", "Gate failed", Some("NIT-2"));
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
    fn concurrent_codex_threads_in_one_folder_are_lanes_of_their_own() {
        // Codex's MCP reports go under its thread's id, the one its turn-end hook uses (NIT-36).
        let (db, dir) = db();
        let t0 = now() - 3 * 3600;
        let cwd = "c:/development/harbor";
        session(&db, "codex:thread-a", cwd, t0, t0 + 1800, "idle");
        session(&db, "codex:thread-b", cwd, t0 + 300, t0 + 2400, "working");
        state(&db, "codex:thread-a", t0, "working");
        state(&db, "codex:thread-a", t0 + 1800, "idle");
        state(&db, "codex:thread-b", t0 + 300, "working");
        event(&db, t0 + 600, "codex:thread-a", "status", "Fixing the parser", None);
        event(&db, t0 + 2400, "codex:thread-b", "status", "Writing tests", None);
        // A keyed report's note is written under the thread too, so the issue is that lane's.
        event(&db, t0 + 600, "codex:thread-a", "status", "Fixing the parser", Some("HAR-7"));
        let a = activity(&Config::default(), &db, t0 - 60, t0 + 3000).unwrap();
        let lane = |id: &str| a.lanes.iter().find(|l| l.session_id == id).unwrap_or_else(|| panic!("{id}: {:?}", a.lanes));
        let at = |id: &str, t: i64| lane(id).segments.iter().find(|s| s.from <= t && t < s.to).map(|s| s.state.clone());
        assert_eq!(at("codex:thread-a", t0 + 2000).as_deref(), Some("idle"), "a's turn ended");
        assert_eq!(at("codex:thread-b", t0 + 2000).as_deref(), Some("working"), "b still works");
        assert_eq!((lane("codex:thread-a").issues.clone(), lane("codex:thread-b").issues.clone()), (vec!["HAR-7".to_string()], vec![]));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_agent_id_written_with_short_names_still_folds_into_its_hook_session() {
        // Before NIT-104 an MCP server in a folder reached by its 8.3 short name wrote its id in that
        // spelling (`claude:c:/progra~1/my-app`); normalize spells it out now (NIT-134). Program
        // Files has a short name where the volume makes them; new folders here get none.
        let Some(program_files) = std::env::var_os("ProgramFiles").map(|p| p.to_string_lossy().into_owned()) else { return };
        let short = crate::paths::short_path(&program_files);
        if !cfg!(windows) || short == program_files {
            return;
        }
        let (db, dir) = db();
        let t0 = now() - 3 * 3600;
        let cwd = format!(r"{short}\my-app");
        let old_id = format!("claude:{}", cwd.replace('\\', "/").to_lowercase());
        session(&db, "claude:hook-1", &cwd, t0, t0 + 1800, "working");
        session(&db, &old_id, &cwd, t0, t0 + 1800, "working");
        state(&db, "claude:hook-1", t0, "working");
        event(&db, t0 + 600, &old_id, "gate_passed", "Gate passed", Some("APP-1"));
        let a = activity(&Config::default(), &db, t0 - 60, t0 + 3000).unwrap();
        let ids: Vec<&str> = a.lanes.iter().map(|l| l.session_id.as_str()).collect();
        assert_eq!(ids, ["claude:hook-1"], "the old agent id is folded in, not a lane of its own");
        assert_eq!(a.lanes[0].marks.iter().map(|m| m.kind.as_str()).collect::<Vec<_>>(), ["gate_pass"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn codex_work_before_its_first_turn_end_and_long_waits_are_kept() {
        let (db, dir) = db();
        let t0 = now() - 3 * 3600;
        let cwd = "c:/code/harbor";
        // Codex reports work over MCP for an hour; its hook session appears at the first turn end.
        session(&db, "codex:c:/code/harbor", cwd, t0, t0 + 3600, "working");
        session(&db, "codex:thread-1", cwd, t0 + 3600, t0 + 3600, "idle");
        state(&db, "codex:c:/code/harbor", t0, "working");
        state(&db, "codex:thread-1", t0 + 3600, "idle");
        event(&db, t0 + 600, "codex:c:/code/harbor", "gate_passed", "Gate passed", None);
        // A Claude session that asked something two hours ago and has waited since.
        session(&db, "claude:w", "C:\\Development\\Nitely", t0, t0 + 1200, "waiting");
        state(&db, "claude:w", t0, "working");
        state(&db, "claude:w", t0 + 1200, "waiting");
        let a = activity(&Config::default(), &db, now() - 3600, now()).unwrap();
        let w = a.lanes.iter().find(|l| l.session_id == "claude:w").expect("still waiting, so still shown");
        assert_eq!(w.segments.iter().map(|s| s.state.as_str()).collect::<Vec<_>>(), ["waiting"]);
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:c:/code/harbor").unwrap();
        assert!(!a.lanes.iter().any(|l| l.session_id == "codex:thread-1"), "the hook session is folded in");
        assert_eq!(codex.segments.first().map(|s| (s.from, s.state.as_str())), Some((t0, "working")));
        assert_eq!(codex.marks.iter().map(|m| m.kind.as_str()).collect::<Vec<_>>(), ["gate_pass"]);
        assert_eq!((codex.state.as_str(), codex.last_seen_at), ("idle", t0 + 3600), "from the member seen last");
        // At the same second, the hook's idle beats the MCP id's working.
        state(&db, "codex:c:/code/harbor", t0 + 3600, "working");
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:c:/code/harbor").unwrap();
        assert!(codex.segments.iter().any(|s| s.from == t0 + 3600 && s.state == "idle"), "{:?}", codex.segments);
        // A report after the turn ended is working again, though the MCP id's own state never changed.
        event(&db, t0 + 4000, "codex:c:/code/harbor", "status", "Next turn", None);
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:c:/code/harbor").unwrap();
        assert!(codex.segments.iter().any(|s| s.from == t0 + 4000 && s.state == "working"), "{:?}", codex.segments);
        // And the next turn end is idle again, though the hook's own state never changed.
        event(&db, t0 + 4600, "codex:thread-1", "turn_end", "Turn finished", None);
        db.conn.execute("UPDATE session SET last_seen_at = ?1 WHERE id = 'codex:thread-1'", [t0 + 4600]).unwrap();
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:c:/code/harbor").unwrap();
        let at = |t: i64| codex.segments.iter().find(|s| s.from <= t && t < s.to).map(|s| s.state.clone());
        assert_eq!((at(t0 + 4300).as_deref(), at(t0 + 4700).as_deref()), (Some("working"), Some("idle")), "{:?}", codex.segments);
        // A window that starts after that report still starts working (NIT-43): the report before
        // it says the MCP id's state again, after the earlier turn end.
        let a = activity(&Config::default(), &db, t0 + 4100, now()).unwrap();
        let codex = a.lanes.iter().find(|l| l.session_id == "codex:c:/code/harbor").unwrap();
        let at = |t: i64| codex.segments.iter().find(|s| s.from <= t && t < s.to).map(|s| s.state.clone());
        assert_eq!((at(t0 + 4100).as_deref(), at(t0 + 4700).as_deref()), (Some("working"), Some("idle")), "{:?}", codex.segments);

        // An idle hook session left from yesterday does not swallow today's work in that folder.
        let y = t0 - 20 * 3600;
        session(&db, "claude:old", "C:\\Code\\Ledger", y, y + 600, "idle");
        session(&db, "claude:c:/code/ledger", "c:/code/ledger", t0, t0 + 1800, "working");
        state(&db, "claude:c:/code/ledger", t0, "working");
        event(&db, t0 + 900, "claude:c:/code/ledger", "gate_passed", "Gate passed", None);
        let a = activity(&Config::default(), &db, t0 - 60, now()).unwrap();
        let ledger = a.lanes.iter().find(|l| l.session_id == "claude:c:/code/ledger").expect("a lane of its own");
        assert_eq!(ledger.marks.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
