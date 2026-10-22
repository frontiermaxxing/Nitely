//! What changed since a reader last looked (NIT-562): the person's decisions and the outcomes of
//! work, from the event table, short and grouped. Each reader (an agent session, or a name a script
//! picks) has a cursor that moves forward when it reads, so the feed is a buffer that empties as it
//! is read. The UserPromptSubmit hook adds a reader's unread updates to its session, so an agent
//! hears what the person did in the app before it answers them.
//!
//! `digest` decides, over plain data; `read` gathers the events and moves the cursor.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::db::{now, Db};

/// One event as the feed reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    pub id: i64,
    pub actor: String,
    pub issue_key: Option<String>,
    pub project: Option<String>,
    pub goal: Option<i64>,
    pub kind: String,
    pub summary: String,
    /// The one agent an event is for, when it names no issue: the asker of an ask about none.
    pub for_reader: Option<String>,
}

/// What a reader is told about.
#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    /// An agent: its own issues and the goals they belong to, and the person's decisions in its
    /// projects.
    Mine { issues: BTreeSet<String>, goals: BTreeSet<i64>, projects: BTreeSet<String> },
    /// A coordinator: everything in these projects.
    Project(BTreeSet<String>),
}

/// The reader's unread updates: lines to show, how many more there were, and the cursor to move to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Digest {
    pub lines: Vec<String>,
    pub more: usize,
    pub cursor: i64,
}

impl Digest {
    /// The block the hook adds to the session, or None when there is nothing new.
    pub fn block(&self) -> Option<String> {
        if self.lines.is_empty() {
            return None;
        }
        let mut text = String::from("Nitely, since you last looked:");
        for l in &self.lines {
            text.push_str("\n- ");
            text.push_str(l);
        }
        if self.more > 0 {
            text.push_str(&format!("\n- and {} more: call `updates`", self.more));
        }
        Some(text)
    }
}

/// The person's own moves, as the event table records them.
const DECISIONS: &[&str] = &["answer", "issue_moved", "issue_edited", "goal_changed", "note", "project_review_mode_changed", "project_focus_changed", "project_yolo_changed", "reviewer_settings_changed", "owner_changed", "issue_created", "ask_closed"];
/// Outcomes of work, whoever's they are: lands, sent-backs, a goal done or dropped.
const OUTCOMES: &[&str] = &["landed", "land_failed", "goal_state"];
/// Who files the recovery issue when the full check on the integration branch fails
/// (verification_backstop.rs): that issue's filing is the failure's news.
const BACKSTOP: &str = "verification-backstop";

/// The longest a line's quote of an event may be.
const QUOTE: usize = 90;

/// The lines a reader is told (NIT-562), from the events after its cursor (oldest first), as its
/// `scope` selects them, at most `cap` lines: the person's decisions and the outcomes of work, never
/// the reader's own events. Lands go on one line; goal moves of the same goal go on one line. The
/// cursor moves past every event read, shown or not.
pub fn digest(events: &[FeedEvent], reader: &str, scope: &Scope, cap: usize) -> Digest {
    let cursor = events.iter().map(|e| e.id).max().unwrap_or(0);
    let relevant = |e: &FeedEvent| -> bool {
        let sent_back = e.kind == "submit" && e.summary.contains("requested changes");
        // The reader's own events are not news, except a review's verdict: a background submit
        // records it under the submitting session, which has moved on by the time it comes.
        if e.actor == reader && !sent_back {
            return false;
        }
        let decision = e.actor == "human" && DECISIONS.contains(&e.kind.as_str());
        let outcome = OUTCOMES.contains(&e.kind.as_str()) || sent_back || (e.actor == BACKSTOP && e.kind == "issue_created");
        // An answer to an ask about no issue: for its asker, and a coordinator; no other agent.
        if let (Some(to), Scope::Mine { .. }) = (&e.for_reader, scope) {
            return decision && to == reader;
        }
        if !decision && !outcome {
            return false;
        }
        match scope {
            Scope::Project(projects) => e.project.as_ref().is_some_and(|p| projects.contains(p)) || (e.project.is_none() && decision),
            Scope::Mine { issues, goals, projects } => {
                let mine = e.issue_key.as_ref().is_some_and(|k| issues.contains(k)) || e.goal.is_some_and(|g| goals.contains(&g));
                mine || (decision && e.project.as_ref().is_none_or(|p| projects.contains(p)))
            }
        }
    };
    let mut lines: Vec<String> = Vec::new();
    let mut landed: Vec<String> = Vec::new();
    let mut goal_moves: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let relevant: Vec<&FeedEvent> = events.iter().filter(|e| relevant(e)).collect();
    // Past the cap, the cursor stops just before the first update not told, so the next read
    // (the `updates` the block points to) starts with it: nothing is skipped.
    let mut cursor = cursor;
    let mut more = 0;
    for (n, e) in relevant.iter().enumerate() {
        let key = e.issue_key.clone().unwrap_or_default();
        let adds_line = match e.kind.as_str() {
            "landed" => landed.is_empty(),
            "goal_changed" => !goal_moves.contains_key(&quote(&e.summary)),
            _ => true,
        };
        if adds_line && lines.len() + usize::from(!landed.is_empty()) + goal_moves.len() >= cap {
            cursor = e.id - 1;
            more = relevant.len() - n;
            break;
        }
        match e.kind.as_str() {
            "landed" => landed.push(key),
            "goal_changed" => goal_moves.entry(quote(&e.summary)).or_default().push(key),
            _ => lines.push(line(e)),
        }
    }
    for (goal, keys) in goal_moves {
        lines.push(format!("The person put {} in {}", keys_line(&keys), goal.trim_start_matches("Goal: ")));
    }
    if !landed.is_empty() {
        lines.push(format!("Landed: {}", keys_line(&landed)));
    }
    Digest { lines, more, cursor }
}

/// The most issue keys one grouped line names; the rest are counted (NIT-566).
const GROUP_KEYS: usize = 6;

/// The keys of a grouped line: the first `GROUP_KEYS`, then how many more.
fn keys_line(keys: &[String]) -> String {
    if keys.len() <= GROUP_KEYS {
        return keys.join(", ");
    }
    format!("{} and {} more", keys[..GROUP_KEYS].join(", "), keys.len() - GROUP_KEYS)
}

/// Whether `read` reads the next page of events (NIT-566): this page was full and told nothing, so
/// an update may come after it. A page that told something, or ran out of events, ends the read.
fn read_on(told: bool, read: usize, page: usize) -> bool {
    !told && read == page
}

/// The cursor a reader keeps for a scope (NIT-562): one per reader, scope and projects, so reading
/// one project's updates never uses up another's.
pub fn cursor_key(reader: &str, projects: &[String], whole: bool) -> String {
    let mut projects: Vec<String> = projects.iter().map(|p| p.to_uppercase()).collect();
    projects.sort();
    format!("{reader} {} {}", if whole { "project" } else { "mine" }, projects.join(","))
}

/// One event in words.
fn line(e: &FeedEvent) -> String {
    let on = e.issue_key.as_deref().map(|k| format!(" on {k}")).unwrap_or_default();
    let said = quote(&e.summary);
    match e.kind.as_str() {
        "answer" => format!("The person answered the ask{on}: \"{said}\""),
        "issue_moved" => format!("The person moved {}: {said}", e.issue_key.as_deref().unwrap_or("an issue")),
        "note" => format!("The person noted{on}: {said}"),
        "issue_created" if e.actor == BACKSTOP => {
            format!("The full check on the integration branch failed; {} tracks it", e.issue_key.as_deref().unwrap_or("a recovery issue"))
        }
        "issue_created" => format!("The person filed {}", e.issue_key.as_deref().unwrap_or("an issue")),
        // The goal's title may be long: it is cut, never its new state.
        "goal_state" => match e.summary.rsplit_once(" is ") {
            Some((goal, state)) => format!("{} is {state}", quote(goal)),
            None => said,
        },
        "submit" => format!("{}: the review asked for changes", e.issue_key.as_deref().unwrap_or("A change")),
        "issue_edited" => {
            // `edit_issue` records what it was: "Title was: ...", "Body was: ...".
            let parts: Vec<&str> = [("Title was", "title"), ("Body was", "requirements")].iter().filter(|(mark, _)| e.summary.contains(mark)).map(|(_, part)| *part).collect();
            let what = if parts.is_empty() { "it".to_string() } else { format!("its {}", parts.join(" and ")) };
            format!("The person edited {what} on {}: read it again", e.issue_key.as_deref().unwrap_or("an issue"))
        }
        "ask_closed" => format!("The person closed the ask{on}"),
        "reviewer_settings_changed" => format!("The person changed the reviewer settings: {said}"),
        "land_failed" => format!("{} was sent back from landing: {said}", e.issue_key.as_deref().unwrap_or("A change")),
        _ => format!("The person changed{on}: {said}"),
    }
}

/// The first line of `text`, at most QUOTE characters.
fn quote(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= QUOTE {
        first.to_string()
    } else {
        format!("{}…", first.chars().take(QUOTE).collect::<String>().trim_end())
    }
}

/// How far back a reader's first read starts: never the whole history.
pub const FIRST_WINDOW_SECS: i64 = 15 * 60;
/// Lines a hook adds at most.
pub const HOOK_CAP: usize = 8;

/// Reads `reader`'s unread updates in `projects` (whole projects when `whole`), at most `cap`
/// lines, and moves its cursor past them.
pub fn read(db: &Db, reader: &str, projects: &[String], whole: bool, cap: usize) -> Result<Digest> {
    read_paged(db, reader, projects, whole, cap, PAGE)
}

/// How many events `read` takes at a time.
const PAGE: usize = 5000;

/// `read`, `page` events at a time: past pages that tell nothing (tool calls, statuses) to the first
/// that does, so an update after many quiet events is told now, not on the next read.
fn read_paged(db: &Db, reader: &str, projects: &[String], whole: bool, cap: usize, page: usize) -> Result<Digest> {
    // At least one line: a cap of 0 would tell nothing and never move past an update (NIT-597 review).
    let cap = cap.max(1);
    let key = cursor_key(reader, projects, whole);
    let cursor: Option<i64> = db.conn.query_row("SELECT event_id FROM update_cursor WHERE reader = ?1", [&key], |r| r.get(0)).optional()?;
    let after = match cursor {
        Some(c) => c,
        None => db.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM event WHERE ts < ?1", [now() - FIRST_WINDOW_SECS], |r| r.get(0))?,
    };
    let mut stmt = db.conn.prepare(
        "SELECT e.id, e.actor, e.issue_key, e.type, COALESCE(e.summary, ''),
                CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.asked_by') END,
                COALESCE(i.project, CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.project') END),
                COALESCE(i.goal, CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.goal') END)
         FROM event e LEFT JOIN issue i ON i.key = e.issue_key COLLATE NOCASE
         WHERE e.id > ?1 ORDER BY e.id LIMIT ?2",
    )?;
    let projects: BTreeSet<String> = projects.iter().map(|p| p.to_uppercase()).collect();
    let scope = if whole { Scope::Project(projects) } else { mine(db, reader, projects)? };
    let mut from = after;
    let digest = loop {
        let events: Vec<FeedEvent> = stmt
            .query_map(params![from, page as i64], |r| {
                Ok(FeedEvent { id: r.get(0)?, actor: r.get(1)?, issue_key: r.get(2)?, kind: r.get(3)?, summary: r.get(4)?, for_reader: r.get(5)?, project: r.get(6)?, goal: r.get(7)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut digest = digest(&events, reader, &scope, cap);
        digest.cursor = digest.cursor.max(from);
        // A page that did not move the cursor is read once: reading it again would end the same.
        if !read_on(!digest.lines.is_empty(), events.len(), page) || digest.cursor == from {
            break digest;
        }
        from = digest.cursor;
    };
    db.conn.execute(
        "INSERT INTO update_cursor (reader, event_id, read_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(reader) DO UPDATE SET event_id = excluded.event_id, read_at = excluded.read_at",
        params![key, digest.cursor, now()],
    )?;
    Ok(digest)
}

/// The UserPromptSubmit hook's output for agent session `reader` working in `cwd` (NIT-562): its
/// unread updates in the project of `cwd` as Claude Code's `additionalContext`, or None when there is
/// nothing new. `NITELY_UPDATES_SCOPE=project` makes a session a coordinator, told about the whole
/// project.
pub fn for_prompt(config: &crate::Config, db: &Db, reader: &str, cwd: &str) -> Result<Option<serde_json::Value>> {
    let projects: Vec<String> = crate::config::resolve_project(config, db, None, cwd).ok().into_iter().collect();
    let whole = std::env::var("NITELY_UPDATES_SCOPE").is_ok_and(|s| s.eq_ignore_ascii_case("project"));
    let digest = read(db, reader, &projects, whole, HOOK_CAP)?;
    Ok(digest.block().map(|text| serde_json::json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": text } })))
}

/// An agent's own issues (those it worked on, asked about or holds) and their goals.
fn mine(db: &Db, reader: &str, projects: BTreeSet<String>) -> Result<Scope> {
    let mut stmt = db.conn.prepare(
        "SELECT DISTINCT i.key, i.goal FROM issue i
         WHERE i.claimed_by = ?1
            OR EXISTS (SELECT 1 FROM event e WHERE e.issue_key = i.key AND e.actor = ?1)
            OR EXISTS (SELECT 1 FROM ask a WHERE a.issue_key = i.key COLLATE NOCASE AND a.asked_by = ?1)",
    )?;
    let rows: Vec<(String, Option<i64>)> = stmt.query_map([reader], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(Scope::Mine { issues: rows.iter().map(|(k, _)| k.clone()).collect(), goals: rows.iter().filter_map(|(_, g)| *g).collect(), projects })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: i64, actor: &str, key: Option<&str>, project: Option<&str>, goal: Option<i64>, kind: &str, summary: &str) -> FeedEvent {
        FeedEvent { id, actor: actor.into(), issue_key: key.map(str::to_string), project: project.map(str::to_string), goal, kind: kind.into(), summary: summary.into(), for_reader: None }
    }

    fn feed() -> Vec<FeedEvent> {
        vec![
            ev(10, "human", Some("APP-70"), Some("APP"), Some(22), "answer", "Go ahead as listed"),
            ev(11, "human", Some("APP-70"), Some("APP"), Some(22), "issue_moved", "Needs You → Todo: answered; it waits for the next agent"),
            ev(12, "claude:other", Some("APP-9"), Some("APP"), None, "landed", "Landed x on main at abc"),
            ev(13, "claude:me", Some("APP-3"), Some("APP"), Some(22), "landed", "Landed y on main at def"),
            ev(14, "claude:other", Some("APP-9"), Some("APP"), None, "status", "Working"),
            ev(15, "human", Some("APP-4"), Some("APP"), None, "goal_changed", "Goal: Less ceremony"),
            ev(16, "human", Some("APP-5"), Some("APP"), None, "goal_changed", "Goal: Less ceremony"),
            ev(17, "human", Some("WEB-1"), Some("WEB"), None, "answer", "No"),
            ev(18, "codex:x", Some("APP-6"), Some("APP"), Some(22), "land_failed", "Land stopped: rebase conflict in a.rs"),
            ev(19, "human", Some("APP-3"), Some("APP"), Some(22), "issue_edited", "Title was: Old\n\nBody was:\nold body"),
            ev(20, "human", None, Some("WEB"), None, "project_focus_changed", "WEB focus: something else"),
            ev(21, "verification-backstop", Some("APP-80"), Some("APP"), None, "issue_created", "Filed as Todo"),
            ev(22, "human", None, Some("APP"), Some(22), "goal_state", "Goal 22 \"Less ceremony\" is done"),
        ]
    }

    #[test]
    fn a_reader_hears_the_persons_decisions_and_its_own_works_outcomes_never_its_own_moves() {
        let mine = Scope::Mine { issues: ["APP-3".to_string(), "APP-70".to_string()].into(), goals: [22].into(), projects: ["APP".to_string()].into() };
        let d = digest(&feed(), "claude:me", &mine, 10);
        assert_eq!(
            d.lines,
            [
                "The person answered the ask on APP-70: \"Go ahead as listed\"",
                "The person moved APP-70: Needs You → Todo: answered; it waits for the next agent",
                "APP-6 was sent back from landing: Land stopped: rebase conflict in a.rs",
                "The person edited its title and requirements on APP-3: read it again",
                "Goal 22 \"Less ceremony\" is done",
                "The person put APP-4, APP-5 in Less ceremony",
            ],
            "another agent's land of an issue outside its goals, its own land, a status, another project's answer and another project's focus are left out"
        );
        assert_eq!((d.more, d.cursor), (0, 22), "the cursor moves past everything read");
        // A coordinator also hears that the full check on main failed (the backstop's recovery issue).
        let whole = digest(&feed(), "claude:coord", &Scope::Project(["APP".to_string()].into()), 20);
        assert!(whole.lines.contains(&"The full check on the integration branch failed; APP-80 tracks it".to_string()), "{:?}", whole.lines);

        // A coordinator reads the whole project: other agents' lands too, grouped on one line.
        let whole = digest(&feed(), "claude:coord", &Scope::Project(["APP".to_string()].into()), 10);
        assert_eq!(whole.lines.last().unwrap(), "Landed: APP-9, APP-3");
        assert!(!whole.lines.iter().any(|l| l.contains("WEB-1")), "only its projects");
    }

    #[test]
    fn the_block_is_capped_and_says_how_to_read_the_rest_and_nothing_new_adds_nothing() {
        let whole = Scope::Project(["APP".to_string()].into());
        let d = digest(&feed(), "claude:coord", &whole, 2);
        assert_eq!((d.lines.len(), d.more), (2, 8), "the eight updates after the first two are left for the next read");
        let block = d.block().unwrap();
        assert!(block.starts_with("Nitely, since you last looked:\n- The person answered the ask on APP-70") && block.ends_with("- and 8 more: call `updates`"), "{block}");
        // The cursor stops before the first update not told: the next read starts there, so the
        // rest are told and none is lost (the review of NIT-562).
        assert_eq!(d.cursor, 11);
        let rest: Vec<FeedEvent> = feed().into_iter().filter(|e| e.id > d.cursor).collect();
        let next = digest(&rest, "claude:coord", &whole, 8);
        assert_eq!(next.lines.len(), 6, "{:?}", next.lines);
        assert_eq!((next.more, next.cursor), (0, 22));
        let quiet = digest(&[ev(20, "claude:other", Some("APP-9"), Some("APP"), None, "status", "Working")], "claude:coord", &whole, 8);
        assert_eq!((quiet.block(), quiet.cursor), (None, 20), "nothing to say, but the cursor still moves");
        assert_eq!(digest(&[], "r", &whole, 8).block(), None);
    }

    /// The hook's round trip over a database (NIT-562): the first prompt after the person answers
    /// gets the block, the next gets nothing, and a new decision shows on the one after.
    #[test]
    fn a_prompt_hears_an_answer_once() {
        let dir = crate::paths::unique_temp("nitely-updates", "");
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("APP", "App").unwrap();
        let agent = crate::issues::Actor::Agent("claude:s1".into());
        let issue = db
            .create_issue(&crate::issues::NewIssue { project: "APP".into(), title: "Pick a way".into(), body: "why".into(), kind: "feature".into(), source: "claude".into(), evidence: "none yet".into(), ..Default::default() }, &agent)
            .unwrap();
        let ask = db.create_ask("Which way?", &["Left".into(), "Right".into()], Some(&issue.key), None, &agent).unwrap();
        db.answer_ask(ask.id, "Left", &crate::issues::Actor::Human).unwrap();
        let first = read(&db, "claude:s1", &["APP".into()], false, HOOK_CAP).unwrap();
        let block = first.block().expect("the answer is news");
        assert!(block.contains(&format!("The person answered the ask on {}: \"Left\"", issue.key)), "{block}");
        assert_eq!(read(&db, "claude:s1", &["APP".into()], false, HOOK_CAP).unwrap().block(), None, "told once");
        db.log_event(&crate::issues::Actor::Human, Some(&issue.key), None, "note", "Prefer the left one").unwrap();
        assert!(read(&db, "claude:s1", &["APP".into()], false, HOOK_CAP).unwrap().block().unwrap().contains("Prefer the left one"));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    /// What the fourth review found missing: an answer to an ask about no issue reaches its asker
    /// only, a review that sent a change back is news, and a long goal title never hides its state.
    #[test]
    fn issue_less_answers_send_backs_and_long_goal_titles() {
        let answer = FeedEvent { for_reader: Some("claude:me".into()), ..ev(1, "human", None, None, None, "answer", "Left") };
        let mine = |_: &str| Scope::Mine { issues: BTreeSet::new(), goals: BTreeSet::new(), projects: ["APP".to_string()].into() };
        assert_eq!(digest(std::slice::from_ref(&answer), "claude:me", &mine("claude:me"), 8).lines, ["The person answered the ask: \"Left\""]);
        assert!(digest(std::slice::from_ref(&answer), "claude:other", &mine("claude:other"), 8).lines.is_empty(), "not another agent's news");
        let sent_back = ev(2, "codex:x", Some("APP-7"), Some("APP"), None, "submit", "Review by claude at its land requested changes: [P1] ...");
        let whole = Scope::Project(["APP".to_string()].into());
        assert_eq!(digest(&[sent_back], "claude:coord", &whole, 8).lines, ["APP-7: the review asked for changes"]);
        let long = format!("Goal 22 \"{}\" is done", "very ".repeat(30));
        let line = &digest(&[ev(3, "human", None, Some("APP"), Some(22), "goal_state", &long)], "claude:coord", &whole, 8).lines[0];
        assert!(line.ends_with(" is done") && line.contains('…'), "{line}");
    }

    /// What the person sent back from the fifth review: a change to the reviewer settings is one of
    /// their decisions, and a background review's send-back reaches the session that submitted it.
    #[test]
    fn reviewer_settings_and_ones_own_send_back_are_news() {
        let mine = Scope::Mine { issues: ["APP-7".to_string()].into(), goals: BTreeSet::new(), projects: ["APP".to_string()].into() };
        let settings = ev(1, "human", None, None, None, "reviewer_settings_changed", "Review settings for app: Saved.");
        assert_eq!(digest(&[settings], "claude:me", &mine, 8).lines, ["The person changed the reviewer settings: Review settings for app: Saved."]);
        let own = [
            ev(2, "claude:me", Some("APP-7"), Some("APP"), None, "submit", "Review by codex requested changes. Pull request: x"),
            ev(3, "claude:me", Some("APP-7"), Some("APP"), None, "submit", "Review by codex passed; ready to land"),
            ev(4, "claude:me", Some("APP-7"), Some("APP"), None, "note", "Fixed it"),
        ];
        assert_eq!(digest(&own, "claude:me", &mine, 8).lines, ["APP-7: the review asked for changes"], "only the send-back, not its own pass or note");
    }

    /// NIT-566: a grouped line names a few keys and counts the rest; a settings change in one
    /// project reaches no reader of another; a read pages past quiet events to the first update.
    #[test]
    fn grouped_lines_are_capped_settings_stay_in_their_project_and_quiet_pages_are_read_past() {
        let whole = Scope::Project(["APP".to_string()].into());
        let lands: Vec<FeedEvent> = (1..=9).map(|n| ev(n, "codex:x", Some(&format!("APP-{n}")), Some("APP"), None, "landed", "Landed")).collect();
        assert_eq!(digest(&lands, "claude:coord", &whole, 8).lines, ["Landed: APP-1, APP-2, APP-3, APP-4, APP-5, APP-6 and 3 more"]);

        let mine = Scope::Mine { issues: BTreeSet::new(), goals: BTreeSet::new(), projects: ["APP".to_string()].into() };
        let elsewhere = ev(1, "human", None, Some("WEB"), None, "reviewer_settings_changed", "Review settings for web: Saved.");
        assert!(digest(std::slice::from_ref(&elsewhere), "claude:me", &mine, 8).lines.is_empty(), "another project's settings");

        assert!(read_on(false, 3, 3), "a full page that told nothing: read on");
        assert!(!read_on(true, 3, 3) && !read_on(false, 2, 3), "something told, or the events ran out");
        let dir = crate::paths::unique_temp("nitely-updates-pages", "");
        let db = Db::open(&dir.join("t.db")).unwrap();
        for _ in 0..5 {
            db.log_event(&crate::issues::Actor::Agent("codex:x".into()), None, None, "status", "Working").unwrap();
        }
        db.log_event(&crate::issues::Actor::Human, None, None, "project_focus_changed", "Focus on APP").unwrap();
        let read = read_paged(&db, "claude:coord", &["APP".into()], true, 8, 3).unwrap();
        assert_eq!(read.lines.len(), 1, "told now, past two quiet pages: {read:?}");
        assert!(read_paged(&db, "claude:coord", &["APP".into()], true, 8, 3).unwrap().lines.is_empty(), "and only once");
        // A cap of 0 still tells the update and ends (NIT-597 review).
        db.log_event(&crate::issues::Actor::Human, None, None, "project_focus_changed", "Focus on APP again").unwrap();
        assert_eq!(read_paged(&db, "claude:coord", &["APP".into()], true, 0, 3).unwrap().lines.len(), 1);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn each_scope_and_project_keeps_its_own_cursor() {
        assert_ne!(cursor_key("cli", &["APP".into()], false), cursor_key("cli", &["WEB".into()], false));
        assert_ne!(cursor_key("cli", &["APP".into()], false), cursor_key("cli", &["APP".into()], true));
        assert_eq!(cursor_key("cli", &["web".into(), "APP".into()], true), cursor_key("cli", &["APP".into(), "WEB".into()], true));
    }

    #[test]
    fn long_quotes_are_cut_to_their_first_line() {
        let long = format!("{}\nsecond line", "word ".repeat(40));
        let q = quote(&long);
        assert!(q.ends_with('…') && q.chars().count() <= QUOTE + 1 && !q.contains("second"), "{q}");
    }
}
