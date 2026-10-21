//! A `nitely mcp` older than the database (a newer build upgraded it) forwards its calls to the
//! newer build once that is installed at its path, waiting a moment for an install under way, and
//! refuses only when nothing newer comes (NIT-327).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// A migration no build has: the database reads as upgraded by a newer Nitely.
const FUTURE: &str = "29991231-from-a-newer-build";

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<Value>,
}

impl Mcp {
    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn call(&mut self, id: i64) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": "list_issues", "arguments": { "project": "APP" } } }));
        self.replies.recv_timeout(Duration::from_secs(60)).expect("an answer")
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// A Nitely home whose database a newer build upgraded, and this build copied to `bin/` (as if
/// installed there), serving MCP from that copy.
fn setup(name: &str, install_wait: u64) -> (Mcp, PathBuf, PathBuf) {
    let base = nitely_core::paths::unique_temp(name, "");
    std::fs::create_dir_all(base.join("bin")).unwrap();
    let db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    drop(db);
    set_future(&base, true);
    let exe = base.join("bin").join(format!("nitely{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_nitely"), &exe).unwrap();
    let mut child = Command::new(&exe)
        .arg("mcp")
        .current_dir(&base)
        .env("NITELY_HOME", &base)
        .env("NITELY_MCP_INSTALL_WAIT", install_wait.to_string())
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
    let mut mcp = Mcp { child, stdin, replies };
    mcp.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "claude-code" } } }));
    assert_eq!(mcp.replies.recv_timeout(Duration::from_secs(20)).unwrap()["id"], 1);
    (mcp, base, exe)
}

/// Adds or removes the migration no build has.
fn set_future(base: &Path, on: bool) {
    let conn = rusqlite::Connection::open(base.join("nitely.db")).unwrap();
    if on {
        conn.execute("INSERT INTO schema_migration (name, applied_at, applied_by) VALUES (?1, 0, 'a newer build')", [FUTURE]).unwrap();
    } else {
        conn.execute("DELETE FROM schema_migration WHERE name = ?1", [FUTURE]).unwrap();
    }
}

/// Puts a "newer" build at `exe` the way `nitely install` does: the running one moved aside, the
/// new one in its place, with a stamp of its own.
fn install_over(exe: &Path) {
    std::fs::rename(exe, exe.with_file_name(format!("nitely-old{}", std::env::consts::EXE_SUFFIX))).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_nitely"), exe).unwrap();
    std::fs::File::options().write(true).open(exe).unwrap().set_modified(SystemTime::now() + Duration::from_secs(60)).unwrap();
}

fn text(reply: &Value) -> String {
    reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

#[test]
fn a_call_that_meets_a_newer_database_waits_for_the_install_and_goes_to_the_new_build() {
    let (mut mcp, base, exe) = setup("nitely-mcp-stale-install", 30);
    // The install lands a moment after the call: the database is the newer build's already.
    let installer = {
        let (base, exe) = (base.clone(), exe.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            // The "newer" build, which knows the database (here: the migration it needs is gone).
            set_future(&base, false);
            install_over(&exe);
        })
    };
    let reply = mcp.call(2);
    installer.join().unwrap();
    assert!(reply["result"]["isError"].is_null(), "{reply}");
    assert!(reply["result"]["_meta"]["nitely_forwarded"].is_string(), "answered by the installed build: {reply}");
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn with_nothing_newer_installed_it_refuses_and_says_so_without_waiting_again() {
    let (mut mcp, base, _exe) = setup("nitely-mcp-stale-none", 1);
    let reply = mcp.call(2);
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    let said = text(&reply);
    assert!(said.contains("newer Nitely") && said.contains("nothing newer is installed"), "{said}");
    // The same build still installed: the next call refuses at once.
    let asked = Instant::now();
    let again = mcp.call(3);
    assert_eq!(again["result"]["isError"], true);
    assert!(asked.elapsed() < Duration::from_secs(2), "no second wait: {:?}", asked.elapsed());
    drop(mcp);
    std::fs::remove_dir_all(base).ok();
}
