//! The wake hook (NIT-765), driven as Claude Code drives it: a fake host runs the real
//! `nitely wake claude` with a PostToolUse payload after a job tool, in the background, and reads
//! what Claude Code would show the session when it exits 2. `pipeline` then counts the polls.

use nitely_core::issues::{Actor, NewIssue};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Claude Code running the hook after `tool` in session `session`, the payload on stdin.
fn after(home: &Path, session: &str, tool: &str, response: serde_json::Value) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .args(["wake", "claude"])
        .current_dir(home)
        .env("NITELY_HOME", home)
        .env("NITELY_WAKE_EVERY_MS", "100")
        .env("NITELY_WAKE_WAIT_SECS", "60")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": session, "cwd": home.to_string_lossy(),
        "tool_name": format!("mcp__nitely__{tool}"), "tool_input": {}, "tool_response": response,
    });
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    child
}

/// The reply as Claude Code hands an MCP tool's text to the hook.
fn reply(text: &str) -> serde_json::Value {
    serde_json::json!([{ "type": "text", "text": text }])
}

/// What the host does when the hook ends: exit 2 wakes the session with stderr; anything else is
/// silent.
fn ended(child: Child) -> (Option<i32>, String) {
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn woken(child: Child) -> String {
    let (code, said) = ended(child);
    assert_eq!(code, Some(2), "{said}");
    said
}

fn waits(child: &mut Child) -> bool {
    std::thread::sleep(Duration::from_millis(600));
    child.try_wait().unwrap().is_none()
}

fn settle(child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn submit_job(db: &nitely_core::Db, issue: &str, by: &str, state: &str) -> i64 {
    db.conn
        .execute(
            "INSERT INTO submit_job (issue_key, worktree, branch, head, requested_by, state, queued_at, ended_at) VALUES (?1, 'w', 'b', 'h', ?2, ?3, 1, CASE WHEN ?3 = 'done' THEN strftime('%s','now') END)",
            [issue, by, state],
        )
        .unwrap();
    db.conn.last_insert_rowid()
}

fn finish(db: &nitely_core::Db, job: i64, state: &str) {
    db.conn
        .execute("UPDATE submit_job SET state = 'done', ended_at = strftime('%s','now'), result = ?2 WHERE id = ?1", rusqlite::params![job, format!(r#"{{"state":"{state}","next":"Call land."}}"#)])
        .unwrap();
}

#[test]
fn a_session_that_ends_its_turn_after_submit_is_woken_when_its_work_ends() {
    let home = nitely_core::paths::unique_temp("nitely-wake-hook", "");
    std::fs::create_dir_all(&home).unwrap();
    let mut db = nitely_core::Db::open(&home.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let me = Actor::Agent("claude:s1".into());
    let new = |db: &mut nitely_core::Db, title: &str| {
        db.create_issue(&NewIssue { project: "APP".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "claude".into(), allow_similar: true, ..Default::default() }, &me).unwrap().key
    };
    let (a, b) = (new(&mut db, "Footer link"), new(&mut db, "Header logo"));

    // Nothing running and nothing in review: the hook exits at once and wakes nobody.
    assert_eq!(ended(after(&home, "s1", "submit_status", reply("{}"))), (Some(0), String::new()));

    // A submit that finished before the hook first looked is still told, once.
    let fast = submit_job(&db, &a, "claude:s1", "running");
    finish(&db, fast, "In Review");
    let said = woken(after(&home, "s1", "submit", reply(&format!("\"Handed in as submit job {fast}: the gate and the review run in the background.\""))));
    assert!(said.contains(&format!("{a}: submitted, now In Review. Call land.")), "{said}");
    assert_eq!(ended(after(&home, "s1", "submit_status", reply("{}"))).0, Some(0), "told once");

    // Two submits run; the first to end wakes the session, and says the other still runs.
    let (one, two) = (submit_job(&db, &a, "claude:s1", "running"), submit_job(&db, &b, "claude:s1", "running"));
    let mut waiter = after(&home, "s1", "submit", reply(&format!("\"Handed in as submit job {two}.\"")));
    assert!(waits(&mut waiter), "it waits while both run");
    finish(&db, one, "In Progress");
    let said = woken(waiter);
    assert!(said.contains(&format!("{a}: submitted, now In Progress")) && said.contains(&format!("Still running: submit job {two}")), "{said}");

    // A land goes on in a process of its own; the session then polls the other submit, and the
    // newer waiter takes over, keeping the land.
    let log = home.join("jobs").join("mcp-land-1.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let still = serde_json::json!({ "still_running": true, "log": log.to_string_lossy(), "next": "still running" });
    let mut land = after(&home, "s1", "land", reply(&still.to_string()));
    assert!(waits(&mut land));
    let mut poll = after(&home, "s1", "submit_status", reply(&format!("\"Submit job {two} of {b} is running (5 s so far).\"")));
    settle(&mut land);
    assert_eq!(ended(land), (Some(0), String::new()), "replaced, so it wakes nobody");
    assert!(waits(&mut poll));
    std::fs::write(log.with_extension("json"), r#"{"ok":{"landed":true}}"#).unwrap();
    let said = woken(poll);
    assert!(said.contains("land: landed.") && said.contains(&format!("Still running: submit job {two}")), "{said}");
    finish(&db, two, "In Review");
    assert!(woken(after(&home, "s1", "submit_status", reply("{}"))).contains(&format!("{b}: submitted, now In Review")));

    // The session claims an issue in review, and another session lands it: the claimer is woken.
    db.conn.execute("UPDATE issue SET claimed_by = 'claude:s1', state = 'In Review' WHERE key = ?1", [&a]).unwrap();
    let mut claimer = after(&home, "s1", "submit_status", reply("{}"));
    assert!(waits(&mut claimer), "it watches while it holds an issue in review");
    // Another session's land, run in a process of its own: no job row, only its event.
    db.log_event(&Actor::Agent("claude:other".into()), Some(&a), None, "landed", &format!("Landed {a} on main at abc123")).unwrap();
    let said = woken(claimer);
    assert!(said.contains(&format!("{a} (your issue, by another session): Landed {a} on main at abc123")), "{said}");
    db.conn.execute("UPDATE issue SET state = 'Done' WHERE key = ?1", [&a]).unwrap();

    // What `pipeline` counts for the landed change: every status poll, and the short ones.
    db.record_status_poll(Some(&a), "submit_status", "claude:s1", 600).unwrap();
    db.record_status_poll(Some(&a), "submit_status", "claude:s1", 0).unwrap();
    db.log_event(&me, Some(&a), None, "landed", "Landed").unwrap();
    let week = nitely_crew::pipeline::read(&db, None, "APP", 1, nitely_core::db::now()).unwrap().weeks.pop().unwrap();
    assert_eq!((week.landed, week.polls.polls, week.polls.short, week.polls.per_landed), (1, 2, 1, Some(2.0)));
    drop(db);
    std::fs::remove_dir_all(home).ok();
}
