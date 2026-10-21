//! `nitely mcp` answers each tool call when it ends, not in the order they came: an agent's long
//! call (a lane run, waiting on a submit) holds up none of its later ones, a call the client
//! cancelled is never answered, and no call waits past the server's cap (NIT-308).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: mpsc::Receiver<Value>,
}

impl Mcp {
    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn call(&mut self, id: i64, name: &str, arguments: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } }));
    }

    /// The next reply within `wait`, if any.
    fn next(&self, wait: Duration) -> Option<Value> {
        self.replies.recv_timeout(wait).ok()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// A Nitely home with a project and a lane run that stays queued (its pid is this test's, which is
/// alive), and a `nitely mcp` on it whose waits are capped at three seconds.
fn setup(name: &str) -> (Mcp, i64, std::path::PathBuf) {
    let base = nitely_core::paths::unique_temp(name, "");
    std::fs::create_dir_all(&base).unwrap();
    let db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    drop(db);
    let conn = rusqlite::Connection::open(base.join("nitely.db")).unwrap();
    conn
        .execute(
            "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, pid, priority)
             VALUES ('APP', 'C:/code/app', NULL, 'claude:x', 'run', 'abc', 'def', 'abc', 'cargo test', 'queued', 'local', ?1, ?2, 3)",
            rusqlite::params![nitely_core::db::now(), std::process::id()],
        )
        .unwrap();
    let run = conn.last_insert_rowid();
    drop(conn);
    let mut child = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .arg("mcp")
        .current_dir(&base)
        .env("NITELY_HOME", &base)
        .env("NITELY_MCP_WAIT_CAP", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, replies) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                tx.send(v).ok();
            }
        }
    });
    let mut mcp = Mcp { child, stdin: Some(stdin), replies };
    mcp.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "claude-code" } } }));
    assert_eq!(mcp.next(Duration::from_secs(20)).unwrap()["id"], 1);
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    (mcp, run, base)
}

fn text(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn external_agents_follow_the_persons_focus_and_cannot_change_it() {
    use nitely_core::goals::GoalChange;
    use nitely_core::issues::{Actor, NewIssue};

    let (mut mcp, _run, base) = setup("nitely-mcp-focus");
    let mut db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    let goal = db.save_goal("APP", None, &GoalChange { title: Some("Welcome".into()), outcome: Some("An easy first run".into()), ..Default::default() }, &Actor::Human).unwrap();
    let urgent = db.create_issue(&NewIssue { project: "APP".into(), title: "Export repair".into(), body: "Repair the export checksum".into(), kind: "bug".into(), evidence: "scenario".into(), source: "human".into(), priority: 1, ..Default::default() }, &Actor::Human).unwrap();
    db.set_project_focus("APP", Some(goal.id), false, &Actor::Human).unwrap();

    mcp.call(20, "get_project", json!({ "project": "APP" }));
    assert_eq!(text(&mcp.next(Duration::from_secs(20)).unwrap())["focus"]["goal"], goal.id);
    mcp.call(21, "next_task", json!({ "project": "APP" }));
    let empty = text(&mcp.next(Duration::from_secs(20)).unwrap());
    assert!(empty["issue"].is_null(), "unrelated urgent work waits: {empty}");
    assert!(empty["message"].as_str().unwrap().to_lowercase().contains("focus"), "{empty}");

    // There is no agent-side focus mutation; the browser preview also refuses the app's write.
    mcp.call(22, "set_project_focus", json!({ "project": "APP", "goal": null, "allowUrgent": true }));
    let refused = mcp.next(Duration::from_secs(20)).unwrap();
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(db.project_focus("APP").unwrap().goal, Some(goal.id));
    let preview = Command::new(env!("CARGO_BIN_EXE_nitely")).args(["ui", "set_project_focus", "{}"])
        .current_dir(&base).env("NITELY_HOME", &base).output().unwrap();
    assert!(!preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stderr).contains("read-only"));

    db.set_project_focus("APP", Some(goal.id), true, &Actor::Human).unwrap();
    mcp.call(23, "next_task", json!({ "project": "APP" }));
    let picked = text(&mcp.next(Duration::from_secs(20)).unwrap());
    assert_eq!(picked["issue"]["key"], urgent.key, "urgent interruption was enabled: {picked}");
    assert_eq!(db.issue(&urgent.key).unwrap().unwrap().priority, 1, "focus never rewrites priority");

    let later = db.create_issue(&NewIssue { project: "APP".into(), title: "Theme picker".into(), body: "Choose the appearance".into(), kind: "feature".into(), evidence: "scenario".into(), source: "human".into(), priority: 3, ..Default::default() }, &Actor::Human).unwrap();
    db.set_project_focus("APP", None, true, &Actor::Human).unwrap();
    mcp.call(24, "next_task", json!({ "project": "APP" }));
    assert_eq!(text(&mcp.next(Duration::from_secs(20)).unwrap())["issue"]["key"], later.key);
    drop(db);
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn agents_cannot_invoke_the_human_reviewer_settings_save() {
    let (mut mcp, _run, base) = setup("nitely-mcp-reviewer-settings");
    mcp.call(2, "reviewer_settings_save", json!({ "repo": "sample", "edit": { "reviewer": { "*": "codex" } }, "preview": {}, "actor": "human" }));
    let reply = mcp.next(Duration::from_secs(20)).unwrap();
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(reply.to_string().contains("unknown tool reviewer_settings_save"), "{reply}");
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

/// A long call holds up none after it and ends at the cap; and one still running when the client
/// leaves finishes, and answers, before the server exits (NIT-528 merged that test in here).
#[test]
fn a_long_call_holds_up_none_after_it_and_ends_at_the_cap() {
    let (mut mcp, run, base) = setup("nitely-mcp-concurrent");
    let asked = Instant::now();
    mcp.call(2, "run_status", json!({ "id": run, "wait_seconds": 600 }));
    mcp.call(3, "list_issues", json!({ "project": "APP" }));
    let first = mcp.next(Duration::from_secs(20)).unwrap();
    assert_eq!(first["id"], 3, "the quick call is answered while the long one waits: {first}");
    let second = mcp.next(Duration::from_secs(20)).unwrap();
    assert_eq!(second["id"], 2);
    let waited = asked.elapsed();
    assert!(waited >= Duration::from_secs(1) && waited < Duration::from_secs(15), "it waits up to the 1 s cap, not the 600 s asked: {waited:?}");
    let status = text(&second);
    assert_eq!(status["state"], "queued");
    assert!(status["next"].as_str().unwrap().contains("call run_status"), "{status}");

    mcp.call(8, "run_status", json!({ "id": run, "wait_seconds": 600 }));
    std::thread::sleep(Duration::from_millis(200));
    let left = Instant::now();
    mcp.stdin.take();
    let answered = mcp.next(Duration::from_secs(20)).expect("the call still ends and answers");
    assert_eq!(answered["id"], 8);
    let exited = mcp.child.wait().unwrap();
    assert!(exited.success(), "{exited:?}");
    assert!(left.elapsed() >= Duration::from_millis(500), "it waited for the call: {:?}", left.elapsed());
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn a_cancelled_call_is_not_answered_and_frees_the_server() {
    let (mut mcp, run, base) = setup("nitely-mcp-cancel");
    let asked = Instant::now();
    mcp.call(4, "run_status", json!({ "id": run, "wait_seconds": 600 }));
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 4, "reason": "the client gave up" } }));
    mcp.call(5, "list_issues", json!({ "project": "APP" }));
    assert_eq!(mcp.next(Duration::from_secs(20)).unwrap()["id"], 5);
    // Past the server's 1 s cap, when the call would have been answered, it ends without a word:
    // half a second past it is enough to see that, not a fixed wait (NIT-544).
    let past_the_cap = Duration::from_millis(1500).saturating_sub(asked.elapsed());
    assert!(mcp.next(past_the_cap).is_none(), "a cancelled call gets no answer");
    mcp.call(6, "list_issues", json!({ "project": "APP" }));
    assert_eq!(mcp.next(Duration::from_secs(20)).unwrap()["id"], 6);
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn a_cancel_for_a_call_not_running_does_not_silence_a_later_one() {
    let (mut mcp, _run, base) = setup("nitely-mcp-stale-cancel");
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 7 } }));
    mcp.call(7, "list_issues", json!({ "project": "APP" }));
    assert_eq!(mcp.next(Duration::from_secs(20)).unwrap()["id"], 7, "the id was free again");
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn a_detached_call_runs_as_the_session_it_was_given_and_writes_its_answer() {
    let (mcp, _run, base) = setup("nitely-mcp-detached");
    drop(mcp);
    let out = base.join("answer.json");
    let request = json!({ "vendor": "claude", "cwd": base.to_string_lossy(), "params": { "name": "report", "arguments": { "text": "Reported from apart" } }, "session": "claude:s1", "out": out.to_string_lossy() });
    let mut child = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .arg("mcp-detached")
        .current_dir(&base)
        .env("NITELY_HOME", &base)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    assert!(child.wait().unwrap().success());
    let answer: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert!(answer.get("ok").is_some() && answer.get("error").is_none(), "{answer}");
    let conn = rusqlite::Connection::open(base.join("nitely.db")).unwrap();
    let seen: i64 = conn.query_row("SELECT COUNT(*) FROM session WHERE id = 'claude:s1'", [], |r| r.get(0)).unwrap();
    assert_eq!(seen, 1, "it acts as the session the server resolved");
    drop(conn);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn review_declarations_use_the_target_project_and_rejected_updates_write_nothing() {
    let (mut mcp, _run, base) = setup("nitely-mcp-review-declarations");
    let db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("WEB", "Website").unwrap();
    db.create_project("LOST", "Disconnected").unwrap();
    let web = base.join("web");
    std::fs::create_dir_all(base.join(".nitely")).unwrap();
    std::fs::create_dir_all(web.join(".nitely")).unwrap();
    std::fs::write(base.join(".nitely/playbook.toml"), "[approval]\nhuman_paths = []\n").unwrap();
    std::fs::write(web.join(".nitely/playbook.toml"), "[[approval.human_paths]]\npattern = '^src/billing/'\nreason = 'Money'\n").unwrap();
    std::fs::write(base.join("config.json"), json!({ "repos": [
        { "name": "app", "root": base, "integration_branch": "main", "project": "APP" },
        { "name": "web", "root": web, "integration_branch": "main", "project": "WEB" }
    ] }).to_string()).unwrap();
    let mut id = 30;
    let mut call = |name: &str, args: Value| {
        id += 1;
        mcp.call(id, name, args);
        let reply = mcp.next(Duration::from_secs(20)).unwrap();
        assert_eq!(reply["id"], id);
        reply
    };
    let filed = call("file_issue", json!({ "project": "WEB", "title": "Protected payment repair", "kind": "bug", "body": "Repair payment flow", "evidence": "scenario", "review_level": "inert", "paths": ["src/billing/pay.ts"] }));
    assert_ne!(filed["result"]["isError"], true, "{filed}");
    let issue = db.issue("WEB-1").unwrap().unwrap();
    assert_eq!(issue.review_level.as_deref(), Some("sensitive"));
    let batch = call("file_issues", json!({ "project": "WEB", "issues": [
        { "ref": "a", "title": "Keyboard menu navigation", "kind": "feature", "body": "Support keys", "evidence": "scenario" },
        { "ref": "b", "title": "Billing tax display", "kind": "feature", "body": "Show tax", "evidence": "scenario", "review_level": "contained", "paths": ["src/billing/tax.ts"] }
    ] }));
    assert_ne!(batch["result"]["isError"], true, "{batch}");
    assert_eq!(db.issue("WEB-2").unwrap().unwrap().review_level.as_deref(), Some("standard"));
    assert_eq!(db.issue("WEB-3").unwrap().unwrap().review_level.as_deref(), Some("sensitive"));
    let events = db.timeline("WEB-1").unwrap().len();
    let rejected = call("update_issue", json!({ "key": "WEB-1", "title": "Wrong title", "body": "Wrong body", "priority": 2, "blocked_by": ["WEB-2"], "review_level": "inert" }));
    assert_eq!(rejected["result"]["isError"], true, "{rejected}");
    let after = db.issue("WEB-1").unwrap().unwrap();
    assert_eq!((after.title, after.body, after.priority, after.blocked_by, after.review_level), (issue.title, issue.body, issue.priority, issue.blocked_by, issue.review_level));
    assert_eq!(db.timeline("WEB-1").unwrap().len(), events);
    let updated = call("update_issue", json!({ "key": "WEB-2", "review_level": "inert", "paths": ["src/billing/menu.ts"] }));
    assert_ne!(updated["result"]["isError"], true, "{updated}");
    assert_eq!(db.issue("WEB-2").unwrap().unwrap().review_level.as_deref(), Some("sensitive"));
    let proposed = call("propose_goal", json!({ "project": "WEB", "sentence": "Improve invoices", "title": "Invoice workflow", "outcome": "Invoices are easy to use", "issues": [
        { "ref": "a", "title": "Invoice export", "kind": "feature", "body": "Export invoices", "evidence": "scenario", "touches": ["src/billing/export.ts"], "review_level": "inert" },
        { "ref": "b", "title": "Receipt search", "kind": "feature", "body": "Find receipts", "evidence": "scenario", "touches": ["src/search.ts"] }
    ] }));
    assert_ne!(proposed["result"]["isError"], true, "{proposed}");
    let proposed = text(&proposed);
    let plan = db.goal_plan_for_issue(proposed["issue"]["key"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(plan.plan.issues[0].review_level.as_deref(), Some("sensitive"));
    assert_eq!(plan.plan.issues[1].review_level.as_deref(), Some("standard"));
    let missing = call("file_issue", json!({ "project": "LOST", "title": "Unknown policy", "kind": "feature", "body": "Unknown", "evidence": "scenario", "review_level": "inert", "paths": ["src/billing/pay.ts"] }));
    assert_eq!(missing["result"]["isError"], true, "{missing}");
    assert!(db.list_issues(Some("LOST"), true).unwrap().is_empty());
    drop(db);
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}
