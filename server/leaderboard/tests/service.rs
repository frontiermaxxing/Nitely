//! The service and Nitely's own client together, on a local instance: what the client builds and
//! signs is what the service takes, and what it refuses stays out.

use std::net::TcpListener;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use nitely_core::leaderboard_report::{self, Profile};
use nitely_core::leaderboard_send::{self, Sharing};
use nitely_leaderboard::{handle, Request, Store};

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A report as the app builds it now, from an empty database.
fn report(name: &str) -> leaderboard_report::Report {
    let base = nitely_core::paths::unique_temp("nitely-lb-service-db", "");
    std::fs::create_dir_all(&base).unwrap();
    let db = nitely_core::db::Db::open(&base.join("t.db")).unwrap();
    let project_of = |_: &str| None;
    let inputs = nitely_core::leaderboard::Inputs { now: now(), utc_offset: 0, projects: &[], tokens: &[], project_of_dir: &project_of };
    let board = nitely_core::leaderboard::stats(&db, &inputs).unwrap();
    drop(db);
    std::fs::remove_dir_all(base).ok();
    leaderboard_report::build(&board, 2, &Profile { display_name: name.into(), github: Some("octocat".into()), ..Default::default() }).unwrap()
}

fn store(dir: &std::path::Path) -> Arc<Store> {
    std::fs::create_dir_all(dir).unwrap();
    Arc::new(Store::open(&dir.join("lb.db")).unwrap())
}

#[test]
fn join_send_view_and_leave_against_a_local_instance() {
    let dir = nitely_core::paths::unique_temp("nitely-lb-service", "");
    let store = store(&dir);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/api", listener.local_addr().unwrap().port());
    {
        let store = store.clone();
        std::thread::spawn(move || nitely_leaderboard::serve(listener, store, "/api".into()));
    }

    let me = Sharing::new(&dir.join("me"));
    let other = Sharing::new(&dir.join("other"));
    let profile = Profile { display_name: "Night Owl".into(), ..Default::default() };
    let status = me.join(&profile, &[], Some(&url)).unwrap();
    let id = status.id.clone().unwrap();
    other.join(&Profile { display_name: "Early Bird".into(), ..Default::default() }, &[], Some(&url)).unwrap();

    me.send_report(&report("Night Owl")).unwrap();
    // A second send the same week replaces the first.
    me.send_report(&report("Night Owl II")).unwrap();
    other.send_report(&report("Early Bird")).unwrap();
    assert!(me.state().unwrap().last_sent_at.is_some());

    let board = me.board("week").unwrap();
    let entries = board["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{board}");
    let mine = entries.iter().find(|e| e["id"] == id.as_str()).expect("my row, by my key");
    assert_eq!(mine["display_name"], "Night Owl II");
    assert_eq!(mine["links"]["github"], "https://github.com/octocat");
    assert_eq!(mine["projects"], 2);
    for period in ["30d", "all"] {
        assert_eq!(me.board(period).unwrap()["entries"].as_array().unwrap().len(), 2, "{period}");
    }

    // Leaving deletes what this install sent, and only that, and forgets the key.
    assert_eq!(me.leave().unwrap(), 1);
    let board = other.board("week").unwrap();
    let names: Vec<&str> = board["entries"].as_array().unwrap().iter().map(|e| e["display_name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Early Bird"]);
    assert_eq!(me.status(now()).unwrap().id, None);
    assert!(me.send_report(&report("Night Owl")).is_err(), "nothing goes out after leaving");
    std::fs::remove_dir_all(dir).ok();
}

/// A signed request, as the client makes it, for `handle`.
fn signed(secret: &[u8; 32], method: &str, path: &str, body: &[u8], time: i64) -> Request {
    let key = leaderboard_send::public_id(secret);
    let sig = SigningKey::from_bytes(secret).sign(leaderboard_send::message(method, path, &key, time, body).as_bytes());
    Request {
        method: method.into(),
        path: path.into(),
        key: Some(key),
        time: Some(time.to_string()),
        signature: Some(URL_SAFE_NO_PAD.encode(sig.to_bytes())),
        body: body.to_vec(),
    }
}

#[test]
fn unsigned_replayed_tampered_or_stale_requests_are_refused() {
    let dir = nitely_core::paths::unique_temp("nitely-lb-refusals", "");
    let store = store(&dir);
    let secret = [7u8; 32];
    let body = serde_json::to_vec(&report("Night Owl")).unwrap();
    let (t, ms) = (now(), now_ms());

    let mut unsigned = signed(&secret, "POST", "/v1/leaderboard/reports", &body, ms);
    unsigned.signature = None;
    assert_eq!(handle(&store, &unsigned, t).0, 401);

    let mut tampered = signed(&secret, "POST", "/v1/leaderboard/reports", &body, ms);
    tampered.body = serde_json::to_vec(&report("Someone Else")).unwrap();
    assert_eq!(handle(&store, &tampered, t).0, 401, "the body is not the one signed");

    let other_path = signed(&secret, "POST", "/v1/leaderboard/reports", &body, ms);
    let delete = Request { method: "DELETE".into(), path: "/v1/leaderboard/entries".into(), body: vec![], ..other_path };
    assert_eq!(handle(&store, &delete, t).0, 401, "a report's signature does not delete");

    assert_eq!(handle(&store, &signed(&secret, "POST", "/v1/leaderboard/reports", &body, ms - 3_600_000), t).0, 401, "an hour old");

    let ok = signed(&secret, "POST", "/v1/leaderboard/reports", &body, ms);
    assert_eq!(handle(&store, &ok, t).0, 200);
    let replay = signed(&secret, "POST", "/v1/leaderboard/reports", &body, ms);
    assert_eq!(handle(&store, &replay, t + 1).0, 409, "the same request again");

    // A report with a field the schema lacks, or one out of range, never gets stored.
    let mut extra: serde_json::Value = serde_json::from_slice(&body).unwrap();
    extra["path"] = "C:/Code/my-app".into();
    let extra = serde_json::to_vec(&extra).unwrap();
    assert_eq!(handle(&store, &signed(&secret, "POST", "/v1/leaderboard/reports", &extra, ms + 2), t).0, 400);
    let mut bad: serde_json::Value = serde_json::from_slice(&body).unwrap();
    bad["periods"][0]["first_pass_rate"] = 3.0.into();
    let bad = serde_json::to_vec(&bad).unwrap();
    assert_eq!(handle(&store, &signed(&secret, "POST", "/v1/leaderboard/reports", &bad, ms + 3), t).0, 400);
    std::fs::remove_dir_all(dir).ok();
}

/// Behind a proxy every request carries the sender's address in a header; the service keeps
/// none of it.
#[test]
fn no_address_is_kept() {
    use std::io::{Read, Write};
    let dir = nitely_core::paths::unique_temp("nitely-lb-no-ip", "");
    let store = store(&dir);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let store = store.clone();
        std::thread::spawn(move || nitely_leaderboard::serve(listener, store, String::new()));
    }
    let secret = [9u8; 32];
    let body = serde_json::to_vec(&report("Night Owl")).unwrap();
    let req = signed(&secret, "POST", "/v1/leaderboard/reports", &body, now_ms());
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST /v1/leaderboard/reports HTTP/1.1\r\nHost: x\r\nCF-Connecting-IP: 203.0.113.77\r\nX-Forwarded-For: 198.51.100.23\r\nX-Nitely-Key: {}\r\nX-Nitely-Time: {}\r\nX-Nitely-Signature: {}\r\nContent-Length: {}\r\n\r\n",
        req.key.unwrap(),
        req.time.unwrap(),
        req.signature.unwrap(),
        body.len()
    )
    .unwrap();
    stream.write_all(&body).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    drop(store);
    let mut stored = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        stored.extend(std::fs::read(entry.path()).unwrap_or_default());
    }
    let stored = String::from_utf8_lossy(&stored);
    assert!(stored.contains("Night Owl"), "the report was stored");
    for ip in ["203.0.113.77", "198.51.100.23", "127.0.0.1"] {
        assert!(!stored.contains(ip), "{ip} was stored");
    }
    std::fs::remove_dir_all(dir).ok();
}
