//! All three readers use the same retained data without starting a collector.
use serde_json::{json, Value};
use std::{io::Write, path::Path, process::{Command, Stdio}};

fn command(home: &Path, args: &[&str], input: Option<Value>) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nitely")).args(args).current_dir(home).env("NITELY_HOME", home)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    if let Some(input) = input { writeln!(child.stdin.take().unwrap(), "{input}").unwrap(); }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}
fn stable(mut report: Value) -> Value {
    for key in ["as_of_ms", "age_ms"] { report.as_object_mut().unwrap().remove(key); }
    report
}
#[test]
fn performance_readers_return_the_same_durable_history_and_export_is_bounded() {
    let home = nitely_core::paths::unique_temp("performance-interfaces", "");
    let dir = home.join("performance");
    let store = nitely_core::performance::Store::open(&dir).unwrap();
    store.configure(&nitely_core::performance::Settings { enabled: false, ..Default::default() }).unwrap();
    let conn = rusqlite::Connection::open(dir.join("history.sqlite")).unwrap();
    let at = nitely_core::performance::wall_ms()-10_000;
    let sample = nitely_core::performance::Sample { wall_ms: at, monotonic_ms: 20_000, boot: Some("boot".into()), session: "finished-worker".into(), metrics: nitely_core::performance::Metrics { cpu_percent: Some(42.0), ..Default::default() }, collection_ms: 1 };
    conn.execute("INSERT INTO samples(wall_ms,json) VALUES(?1,?2)", rusqlite::params![at,serde_json::to_string(&sample).unwrap()]).unwrap();
    drop(conn); drop(store);
    let from = (at-60_000).to_string(); let to = at.to_string();
    let query = json!({"view":"history","from_ms":at-60_000,"to_ms":at,"limit":5});
    let cli = command(&home, &["performance","history","--from-ms",&from,"--to-ms",&to,"--limit","5"], None);
    let preview = command(&home, &["ui","performance_query",&json!({"query":query}).to_string()], None);
    let mcp = command(&home, &["mcp-call"], Some(json!({"vendor":"codex","cwd":home,"params":{"name":"performance","arguments":query}})));
    assert!(mcp.get("isError").is_none(), "{mcp}");
    let mcp: Value = serde_json::from_str(mcp["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(stable(cli.clone()), stable(preview));
    assert_eq!(stable(cli.clone()), stable(mcp));
    assert_eq!(cli["status"], "paused");
    assert_eq!(cli["points"][0]["values"]["cpu_percent"], 42.0);
    assert_eq!(cli["latest"]["session"], "finished-worker");
    let export = home.join("diagnostic.json");
    command(&home, &["performance","export","--from-ms",&from,"--to-ms",&to,"--limit","5","--output",export.to_str().unwrap()], None);
    let saved = std::fs::read(&export).unwrap();
    assert!(saved.len()<20_000);
    assert_eq!(stable(cli), stable(serde_json::from_slice(&saved).unwrap()));
    let retry = Command::new(env!("CARGO_BIN_EXE_nitely")).args(["performance","export","--output",export.to_str().unwrap()]).env("NITELY_HOME",&home).current_dir(&home).output().unwrap();
    assert!(!retry.status.success());
    assert_eq!(std::fs::read(&export).unwrap(), saved);
    let store = nitely_core::performance::Store::read(&dir).unwrap().unwrap();
    assert!(store.sessions().unwrap().is_empty(), "reader calls never start a collector");
    drop(store);
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn performance_project_filters_keep_recorded_keys_after_a_project_rename() {
    use nitely_core::performance_attribution::{Context, Identity, Workload};
    use nitely_core::performance_events::{Event, Kind};
    let home = nitely_core::paths::unique_temp("performance-renamed-project", "");
    let dir = home.join("performance");
    let store = nitely_core::performance::Store::open(&dir).unwrap();
    store.configure(&nitely_core::performance::Settings { enabled: false, ..Default::default() }).unwrap();
    let db = nitely_core::Db::open(&home.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    drop(db);
    let at = nitely_core::performance::wall_ms()-10_000;
    let workload = Workload { id: "finished-build".into(), kind: "build".into(), context: Context { project: Some("APP".into()), issue: Some("APP-1".into()), ..Default::default() }, root: Identity { boot: "boot".into(), pid: 1, started: "1".into() }, start_ms: at-5000, end_ms: Some(at), end_observed: false, job_accounting: None };
    let event = Event { wall_ms: at, boot: Some("boot".into()), kind: Kind::Ended, project: "APP".into(), issue: Some("APP-1".into()), run: 1, memory_cap_bytes: None, trim: None };
    let conn = rusqlite::Connection::open(dir.join("history.sqlite")).unwrap();
    conn.execute("INSERT INTO workloads(id,json,ended) VALUES(?1,?2,?3)", rusqlite::params![workload.id,serde_json::to_string(&workload).unwrap(),at]).unwrap();
    conn.execute("INSERT INTO performance_event(wall_ms,project,run,kind,json) VALUES(?1,'APP',1,'ended',?2)", rusqlite::params![at,serde_json::to_string(&event).unwrap()]).unwrap();
    drop(conn); drop(store);
    command(&home, &["project", "rekey", "APP", "NEW"], None);
    let db = nitely_core::Db::open(&home.join("nitely.db")).unwrap();
    assert_eq!(db.project_aliases().unwrap().get("APP").map(String::as_str), Some("NEW"));
    drop(db);
    let from = (at-60_000).to_string(); let to = at.to_string();
    for (project, count) in [("APP",1),("NEW",0)] {
        let query = json!({"view":"history","from_ms":at-60_000,"to_ms":at,"limit":5,"project":project});
        let cli = command(&home, &["performance","history","--from-ms",&from,"--to-ms",&to,"--limit","5","--project",project], None);
        let preview = command(&home, &["ui","performance_query",&json!({"query":query}).to_string()], None);
        let mcp = command(&home, &["mcp-call"], Some(json!({"vendor":"codex","cwd":home,"params":{"name":"performance","arguments":query}})));
        assert!(mcp.get("isError").is_none(), "{mcp}");
        let mcp: Value = serde_json::from_str(mcp["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(stable(cli.clone()), stable(preview));
        assert_eq!(stable(cli.clone()), stable(mcp));
        assert_eq!(cli["project"], project);
        assert_eq!(cli["workloads"].as_array().unwrap().len(), count);
        assert_eq!(cli["events"].as_array().unwrap().len(), count);
    }
    std::fs::remove_dir_all(home).unwrap();
}
