//! The wake hook (NIT-765), driven as Claude Code drives it: a fake host runs the real
//! `nitely wake claude` with a PostToolUse payload after `submit`, and reads what Claude Code would
//! show the session when it exits 2. The session is woken once, when its submit ends, by the newest
//! waiter, and not at all when nothing is running. `pipeline` then counts the polls it was spared.

use nitely_core::issues::{Actor, NewIssue};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Claude Code running the hook after `tool`: the payload on stdin, in the background.
fn after(home: &Path, tool: &str, response: serde_json::Value) -> Child {
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
        "hook_event_name": "PostToolUse", "session_id": "s1", "cwd": home.to_string_lossy(),
        "tool_name": format!("mcp__nitely__{tool}"), "tool_input": {}, "tool_response": response,
    });
    child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
    child
}

/// What the host does when the hook ends: exit 2 wakes the session with stderr; anything else is
/// silent.
fn ended(child: Child) -> (Option<i32>, String) {
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn running(child: &mut Child) -> bool {
    child.try_wait().unwrap().is_none()
}

fn settle(child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(20);
    while running(child) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_session_that_ends_its_turn_after_submit_is_woken_once_when_the_submit_ends() {
    let home = nitely_core::paths::unique_temp("nitely-wake-hook", "");
    std::fs::create_dir_all(&home).unwrap();
    let mut db = nitely_core::Db::open(&home.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let me = Actor::Agent("claude:s1".into());
    let issue = db
        .create_issue(&NewIssue { project: "APP".into(), title: "Footer link".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "claude".into(), allow_similar: true, ..Default::default() }, &me)
        .unwrap();
    let handed_in = serde_json::json!([{ "type": "text", "text": "\"Handed in as submit job 1: the gate and the review run in the background.\"" }]);

    // Nothing running: the hook exits at once and wakes nobody.
    let (code, said) = ended(after(&home, "submit", handed_in.clone()));
    assert_eq!((code, said.as_str()), (Some(0), ""));

    // A submit runs in the background; the session ends its turn, and the hook waits.
    db.conn
        .execute(
            "INSERT INTO submit_job (issue_key, worktree, branch, head, requested_by, state, queued_at) VALUES (?1, 'w', 'b', 'h', 'claude:s1', 'running', 1)",
            [&issue.key],
        )
        .unwrap();
    let mut first = after(&home, "submit", handed_in.clone());
    std::thread::sleep(Duration::from_millis(600));
    assert!(running(&mut first), "it waits while the submit runs");

    // The session polls once anyway: the newer waiter takes over, and the older one goes quietly.
    let mut second = after(&home, "submit_status", serde_json::json!("Submit job 1 is running"));
    settle(&mut first);
    assert_eq!(ended(first), (Some(0), String::new()), "replaced, so it wakes nobody");
    assert!(running(&mut second));

    // The submit ends: the session is woken with what it came to.
    db.conn
        .execute("UPDATE submit_job SET state = 'done', ended_at = 2, result = ?1", [r#"{"state":"In Review","next":"Call land."}"#])
        .unwrap();
    let (code, said) = ended(second);
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains(&format!("{}: submitted, now In Review. Call land.", issue.key)), "{said}");

    // A land that went on in a process of its own: woken when its answer is written.
    let log = home.join("jobs").join("mcp-land-1.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let still = serde_json::json!({ "still_running": true, "log": log.to_string_lossy(), "next": "still running" });
    let mut land = after(&home, "land", serde_json::json!([{ "type": "text", "text": still.to_string() }]));
    std::thread::sleep(Duration::from_millis(600));
    assert!(running(&mut land));
    std::fs::write(log.with_extension("json"), r#"{"ok":{"landed":true}}"#).unwrap();
    let (code, said) = ended(land);
    assert_eq!(code, Some(2), "{said}");
    assert!(said.contains("land: landed."), "{said}");

    // What `pipeline` counts for the landed change: every status poll, and the short ones.
    db.record_status_poll(Some(&issue.key), "submit_status", "claude:s1", 600).unwrap();
    db.record_status_poll(Some(&issue.key), "submit_status", "claude:s1", 0).unwrap();
    db.log_event(&me, Some(&issue.key), None, "landed", "Landed").unwrap();
    let week = nitely_crew::pipeline::read(&db, None, "APP", 1, nitely_core::db::now()).unwrap().weeks.pop().unwrap();
    assert_eq!((week.landed, week.polls.polls, week.polls.short, week.polls.per_landed), (1, 2, 1, Some(2.0)));
    drop(db);
    std::fs::remove_dir_all(home).ok();
}
