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
        .env("NITELY_MCP_WAIT_CAP", "3")
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
    assert!(waited >= Duration::from_secs(3) && waited < Duration::from_secs(15), "it waits up to the cap, not the 600 s asked: {waited:?}");
    let status = text(&second);
    assert_eq!(status["state"], "queued");
    assert!(status["next"].as_str().unwrap().contains("call run_status"), "{status}");
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn a_cancelled_call_is_not_answered_and_frees_the_server() {
    let (mut mcp, run, base) = setup("nitely-mcp-cancel");
    mcp.call(4, "run_status", json!({ "id": run, "wait_seconds": 600 }));
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 4, "reason": "the client gave up" } }));
    mcp.call(5, "list_issues", json!({ "project": "APP" }));
    assert_eq!(mcp.next(Duration::from_secs(20)).unwrap()["id"], 5);
    // Past the cap, the cancelled call ends without a word.
    assert!(mcp.next(Duration::from_secs(6)).is_none(), "a cancelled call gets no answer");
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
fn a_call_still_running_when_the_client_leaves_finishes_before_the_server_exits() {
    let (mut mcp, run, base) = setup("nitely-mcp-leave");
    mcp.call(8, "run_status", json!({ "id": run, "wait_seconds": 600 }));
    std::thread::sleep(Duration::from_millis(500));
    let left = Instant::now();
    mcp.stdin.take();
    let answered = mcp.next(Duration::from_secs(20)).expect("the call still ends and answers");
    assert_eq!(answered["id"], 8);
    let status = mcp.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(left.elapsed() >= Duration::from_secs(2), "it waited for the call: {:?}", left.elapsed());
    std::fs::remove_dir_all(base).ok();
}
