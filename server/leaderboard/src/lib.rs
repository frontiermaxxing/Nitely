//! The Leaderboard service. It takes weekly reports signed with each install's ed25519 key (the
//! public key is the install's only id: no account, no email), keeps them in SQLite, and serves
//! the board per period as JSON. A signed request deletes everything a key sent.
//!
//! It never reads, logs or stores the address a request came from, nor any header a proxy adds
//! with one (`CF-Connecting-IP`, `X-Forwarded-For`): a request is its method, path, the three
//! signing headers and its body, and that is all the handler looks at.
//!
//! The wire format (the client's side is `nitely_crew::leaderboard_send`):
//! - `X-Nitely-Key`: the public key, base64url without padding;
//! - `X-Nitely-Time`: Unix milliseconds, within `WINDOW` of the service's clock and later than the
//!   last request the key made, so a request can't be replayed;
//! - `X-Nitely-Signature`: base64url ed25519 over
//!   `nitely-leaderboard-v1\n{METHOD}\n{path}\n{key}\n{time}\n{hex sha256 of the body}`, where the
//!   path is the one after the service's base (`/v1/leaderboard/reports`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The report schema this service takes.
pub const SCHEMA: u32 = 1;
/// How far a request's time may be from the service's clock, in seconds.
pub const WINDOW: i64 = 300;
/// Reports older than this are dropped.
pub const RETENTION: i64 = 371 * 86_400;
/// The weeks a report's series covers.
pub const SERIES_WEEKS: usize = 26;
/// The periods a board is built for.
pub const PERIODS: [&str; 3] = ["week", "30d", "all"];
const VENDORS: &[&str] = &["claude", "codex", "cursor", "gemini", "copilot", "other"];
const MAX_BODY: usize = 64 << 10;
const MAX_HEAD: usize = 8 << 10;
const MAX_CONNECTIONS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

// ---- The report, exactly as the client builds it (nitely_crew::leaderboard_report) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub schema: u32,
    pub display_name: String,
    pub links: Links,
    /// `github` or `x`: which link gives the picture (NIT-630). Pages build its address from the
    /// handle; nothing here stores or fetches an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    pub week: String,
    pub projects: u64,
    pub periods: Vec<Period>,
    pub weekly: Vec<u64>,
    pub records: Records,
    /// Sent on Sunday evening: the week's final report, which the board keeps as the week's
    /// (NIT-629). Left out when false, so a report that does not close its week reads as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closes_week: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Links {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub period: String,
    pub landed: u64,
    pub landed_per_day: f64,
    pub reviewed: u64,
    pub first_pass_rate: Option<f64>,
    pub lead_time: Option<Durations>,
    pub agent_hours: f64,
    pub tokens: u64,
    pub inbox_reply: Option<Durations>,
    pub steadiest_week: Option<Week>,
    pub vendors: Vec<Vendor>,
    pub review_rounds: Option<Rounds>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Durations {
    pub median: i64,
    pub p90: i64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Week {
    pub landed: u64,
    pub active_days: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vendor {
    pub vendor: String,
    pub landed: u64,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rounds {
    pub mean: f64,
    pub max: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub best_day: u64,
    pub best_week: u64,
    pub longest_streak_days: u64,
    pub fastest_lead: Option<i64>,
}

/// Why a report is refused, beyond its shape.
pub fn check(r: &Report, now: i64) -> std::result::Result<(), String> {
    if r.schema != SCHEMA {
        return Err(format!("this service takes schema {SCHEMA}"));
    }
    let name = r.display_name.trim();
    if name.is_empty() || name != r.display_name || name.chars().count() > 40 || name.chars().any(char::is_control) || name.contains('@') {
        return Err("display_name: 1 to 40 characters, no @".into());
    }
    let handle = |v: &Option<String>, prefix: &str, max: usize, ok: fn(char) -> bool| {
        v.as_deref().is_none_or(|v| v.strip_prefix(prefix).is_some_and(|h| !h.is_empty() && h.len() <= max && h.chars().all(ok)))
    };
    if !handle(&r.links.github, "https://github.com/", 39, |c| c.is_ascii_alphanumeric() || c == '-')
        || !handle(&r.links.x, "https://x.com/", 15, |c| c.is_ascii_alphanumeric() || c == '_')
        || !r.links.website.as_deref().is_none_or(|w| {
            let host = w.strip_prefix("https://").map(|r| r.split(['/', '?', '#']).next().unwrap_or(""));
            w.len() <= 200 && w.chars().all(|c| c.is_ascii_graphic()) && host.is_some_and(|h| h.contains('.') && !h.contains('@'))
        })
    {
        return Err("links: a GitHub or X user name and an https website".into());
    }
    match r.avatar.as_deref() {
        None => {}
        Some("github") if r.links.github.is_some() => {}
        Some("x") if r.links.x.is_some() => {}
        _ => return Err("avatar: github or x, with that link given".into()),
    }
    let this = week_of(now);
    if !valid_week(&r.week) || r.week.as_str() > week_of(now + 86_400).as_str() || r.week.as_str() < week_of(now - 8 * 86_400).as_str() {
        return Err(format!("week: this week or last (it is {this} here)"));
    }
    if r.projects > 1_000 || r.weekly.len() != SERIES_WEEKS || r.weekly.iter().any(|n| *n > 100_000) {
        return Err("projects or weekly out of range".into());
    }
    let names: Vec<&str> = r.periods.iter().map(|p| p.period.as_str()).collect();
    if names != PERIODS {
        return Err("periods: week, 30d and all".into());
    }
    for p in &r.periods {
        let rate_ok = |x: Option<f64>| x.is_none_or(|x| (0.0..=1.0).contains(&x));
        let dur_ok = |d: &Option<Durations>| d.as_ref().is_none_or(|d| d.median >= 0 && d.p90 >= d.median && d.count > 0);
        let finite = [p.landed_per_day, p.agent_hours].iter().all(|x| x.is_finite() && *x >= 0.0);
        if !finite
            || !rate_ok(p.first_pass_rate)
            || p.reviewed > p.landed
            || !dur_ok(&p.lead_time)
            || !dur_ok(&p.inbox_reply)
            || p.vendors.len() > VENDORS.len()
            || p.vendors.iter().any(|v| !VENDORS.contains(&v.vendor.as_str()) || v.landed > p.landed || !(0.0..=1.0).contains(&v.share))
            || p.steadiest_week.as_ref().is_some_and(|w| w.active_days > 7)
            || p.review_rounds.as_ref().is_some_and(|r| !r.mean.is_finite() || r.mean < 0.0)
        {
            return Err(format!("period {}: a number out of range", p.period));
        }
    }
    Ok(())
}

fn valid_week(w: &str) -> bool {
    let b = w.as_bytes();
    b.len() == 8 && b[..4].iter().all(u8::is_ascii_digit) && &b[4..6] == b"-W" && b[6..].iter().all(u8::is_ascii_digit)
}

/// The ISO week of a UTC instant, like `2026-W43`.
pub fn week_of(t: i64) -> String {
    use chrono::Datelike;
    let w = chrono::DateTime::from_timestamp(t, 0).unwrap_or_default().date_naive().iso_week();
    format!("{}-W{:02}", w.year(), w.week())
}

/// The message a request's signature covers.
pub fn message(method: &str, path: &str, key: &str, time: i64, body: &[u8]) -> String {
    let digest: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    format!("nitely-leaderboard-v1\n{method}\n{path}\n{key}\n{time}\n{digest}")
}

// ---- The store ----

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS entry (
               key TEXT NOT NULL, week TEXT NOT NULL, report TEXT NOT NULL, received_at INTEGER NOT NULL,
               PRIMARY KEY (key, week));
             CREATE TABLE IF NOT EXISTS seen (key TEXT PRIMARY KEY, last_time INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS board (period TEXT PRIMARY KEY, json TEXT NOT NULL);",
        )?;
        let store = Store { conn: Mutex::new(conn) };
        store.rebuild(chrono::Utc::now().timestamp())?;
        Ok(store)
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Checks a request's signature and time, and records the time so it can't be sent again;
    /// the key that signed it.
    pub fn verify(&self, req: &Request, now: i64) -> std::result::Result<String, (u16, String)> {
        let refused = |m: &str| (401, m.to_string());
        let (Some(key), Some(time), Some(signature)) = (&req.key, &req.time, &req.signature) else {
            return Err(refused("sign the request: X-Nitely-Key, X-Nitely-Time and X-Nitely-Signature"));
        };
        let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(key).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| refused("X-Nitely-Key is not a public key"))?;
        let public = VerifyingKey::from_bytes(&bytes).map_err(|_| refused("X-Nitely-Key is not a public key"))?;
        let time: i64 = time.parse().map_err(|_| refused("X-Nitely-Time is not a time"))?;
        if (time - now * 1000).abs() > WINDOW * 1000 {
            return Err(refused("X-Nitely-Time is too far from this service's clock"));
        }
        let sig: [u8; 64] = URL_SAFE_NO_PAD.decode(signature).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| refused("X-Nitely-Signature is not a signature"))?;
        public.verify(message(&req.method, &req.path, key, time, &req.body).as_bytes(), &Signature::from_bytes(&sig)).map_err(|_| refused("the signature does not match"))?;
        let conn = self.conn();
        let last: Option<i64> = conn.query_row("SELECT last_time FROM seen WHERE key = ?1", [key], |r| r.get(0)).optional().map_err(server_error)?;
        if last.is_some_and(|l| time <= l) {
            return Err((409, "this request was already made; sign a new one".into()));
        }
        conn.execute("INSERT INTO seen (key, last_time) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET last_time = ?2", params![key, time]).map_err(server_error)?;
        // Past the window a replay is refused on its time alone.
        conn.execute("DELETE FROM seen WHERE last_time < ?1", [(now - 2 * WINDOW) * 1000]).map_err(server_error)?;
        Ok(key.clone())
    }

    /// Keeps `report` as `key`'s for its week, replacing one sent earlier that week, unless that
    /// one closed the week (NIT-629: Sunday evening's send is the week's last word).
    pub fn put(&self, key: &str, report: &Report, now: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO entry (key, week, report, received_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (key, week) DO UPDATE SET report = ?3, received_at = ?4
             WHERE COALESCE(json_extract(entry.report, '$.closes_week'), 0) = 0",
            params![key, report.week, serde_json::to_string(report)?, now],
        )?;
        self.rebuild(now)
    }

    /// Removes everything `key` sent; how many weekly reports that was.
    pub fn delete(&self, key: &str, now: i64) -> Result<usize> {
        let n = self.conn().execute("DELETE FROM entry WHERE key = ?1", [key])?;
        self.rebuild(now)?;
        Ok(n)
    }

    /// The board for `period`, as last built.
    pub fn board(&self, period: &str) -> Result<Option<String>> {
        Ok(self.conn().query_row("SELECT json FROM board WHERE period = ?1", [period], |r| r.get(0)).optional()?)
    }

    /// Drops what is past retention and rebuilds every period's board.
    pub fn rebuild(&self, now: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM entry WHERE received_at < ?1", [now - RETENTION])?;
        let mut rows: Vec<(String, String, Report, i64)> = Vec::new();
        let mut stmt = conn.prepare("SELECT key, week, report, received_at FROM entry ORDER BY week, received_at")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
            let (key, week, report, at) = row?;
            if let Ok(report) = serde_json::from_str(&report) {
                rows.push((key, week, report, at));
            }
        }
        drop(stmt);
        let latest_week = rows.iter().map(|(_, w, _, _)| w.clone()).filter(|w| w.as_str() <= week_of(now + 86_400).as_str()).max();
        // Each key's latest report, for 30 days and all time.
        let mut latest: std::collections::BTreeMap<&str, &(String, String, Report, i64)> = Default::default();
        for row in &rows {
            latest.insert(row.0.as_str(), row);
        }
        let month_ago = week_of(now - 35 * 86_400);
        for period in PERIODS {
            let chosen: Vec<&(String, String, Report, i64)> = match period {
                "week" => rows.iter().filter(|r| Some(&r.1) == latest_week.as_ref()).collect(),
                "30d" => latest.values().copied().filter(|r| r.1 >= month_ago).collect(),
                _ => latest.values().copied().collect(),
            };
            let mut entries: Vec<Value> = chosen.iter().filter_map(|(key, _, r, at)| entry(key, r, period, *at)).collect();
            entries.sort_by(|a, b| {
                let landed = |v: &Value| v["landed"].as_u64().unwrap_or(0);
                landed(b).cmp(&landed(a)).then_with(|| a["display_name"].as_str().cmp(&b["display_name"].as_str()))
            });
            let board = json!({
                "schema": SCHEMA,
                "period": period,
                "week": if period == "week" { latest_week.clone() } else { Some(week_of(now)) },
                "generated_at": now,
                "entries": entries,
            });
            conn.execute("INSERT INTO board (period, json) VALUES (?1, ?2) ON CONFLICT (period) DO UPDATE SET json = ?2", params![period, board.to_string()])?;
        }
        Ok(())
    }
}

/// One row of a board: who (the key, the name and links they chose), the period's numbers, when
/// the report came, and whether it closed its week.
fn entry(key: &str, r: &Report, period: &str, received_at: i64) -> Option<Value> {
    let p = r.periods.iter().find(|p| p.period == period)?;
    let mut v = serde_json::to_value(p).ok()?;
    let o = v.as_object_mut()?;
    o.remove("period");
    o.insert("id".into(), json!(key));
    o.insert("display_name".into(), json!(r.display_name));
    o.insert("links".into(), serde_json::to_value(&r.links).ok()?);
    if let Some(a) = &r.avatar {
        o.insert("avatar".into(), json!(a));
    }
    o.insert("projects".into(), json!(r.projects));
    o.insert("week".into(), json!(r.week));
    o.insert("weekly".into(), json!(r.weekly));
    o.insert("records".into(), serde_json::to_value(&r.records).ok()?);
    o.insert("updated_at".into(), json!(received_at));
    o.insert("closed".into(), json!(r.closes_week));
    Some(v)
}

fn server_error(e: impl std::fmt::Display) -> (u16, String) {
    eprintln!("store error: {e}");
    (500, "the service could not save that".into())
}

// ---- HTTP ----

/// A request as the handler sees it: nothing about where it came from.
pub struct Request {
    pub method: String,
    pub path: String,
    pub key: Option<String>,
    pub time: Option<String>,
    pub signature: Option<String>,
    pub body: Vec<u8>,
}

/// Answers one request: (status, content type, body).
pub fn handle(store: &Store, req: &Request, now: i64) -> (u16, &'static str, String) {
    let error = |status: u16, m: &str| (status, "application/json", json!({ "error": m }).to_string());
    let signed = || store.verify(req, now);
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/leaderboard/health") => (200, "application/json", json!({ "ok": true, "schema": SCHEMA }).to_string()),
        ("GET", path) if path.starts_with("/v1/leaderboard/board/") => {
            let period = path.trim_start_matches("/v1/leaderboard/board/").trim_end_matches(".json");
            match PERIODS.contains(&period).then(|| store.board(period)) {
                Some(Ok(Some(json))) => (200, "application/json", json),
                Some(Ok(None)) | None => error(404, "boards: week, 30d, all"),
                Some(Err(e)) => error(500, &server_error(e).1),
            }
        }
        ("POST", "/v1/leaderboard/reports") => {
            // The shape first, so an unsigned junk body costs no signature check or write.
            let report: Report = match serde_json::from_slice(&req.body) {
                Ok(r) => r,
                Err(e) => return error(400, &format!("not a schema {SCHEMA} report: {e}")),
            };
            if let Err(m) = check(&report, now) {
                return error(400, &m);
            }
            let key = match signed() {
                Ok(k) => k,
                Err((s, m)) => return error(s, &m),
            };
            match store.put(&key, &report, now) {
                Ok(()) => (200, "application/json", json!({ "ok": true, "week": report.week }).to_string()),
                Err(e) => error(500, &server_error(e).1),
            }
        }
        ("DELETE", "/v1/leaderboard/entries") => {
            let key = match signed() {
                Ok(k) => k,
                Err((s, m)) => return error(s, &m),
            };
            match store.delete(&key, now) {
                Ok(n) => (200, "application/json", json!({ "deleted": n }).to_string()),
                Err(e) => error(500, &server_error(e).1),
            }
        }
        (_, "/v1/leaderboard/reports" | "/v1/leaderboard/entries" | "/v1/leaderboard/health") => error(405, "method not allowed"),
        _ => error(404, "not found"),
    }
}

/// Serves `store` on `listener` until it fails. `base` is a path prefix the proxy may leave on
/// (`/api`); requests are matched with it taken off.
pub fn serve(listener: TcpListener, store: Arc<Store>, base: String) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            respond(&mut stream, 503, "application/json", r#"{"error":"busy, try again shortly"}"#, false);
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (store, base, active) = (store.clone(), base.clone(), active.clone());
        std::thread::spawn(move || {
            let (status, kind, body, public) = match read_request(&mut stream, &base) {
                Ok(req) => {
                    let (status, kind, body) = handle(&store, &req, chrono::Utc::now().timestamp());
                    (status, kind, body, status == 200 && req.method == "GET" && req.path.starts_with("/v1/leaderboard/board/"))
                }
                Err((status, m)) => (status, "application/json", json!({ "error": m }).to_string(), false),
            };
            respond(&mut stream, status, kind, &body, public);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn read_request(stream: &mut TcpStream, base: &str) -> std::result::Result<Request, (u16, &'static str)> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err((431, "headers too large"));
        }
        read_some(stream, &mut chunk, &mut buf, deadline)?;
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| (400, "headers are not text"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    let path = path.strip_prefix(base).filter(|p| p.starts_with('/')).unwrap_or(path).to_string();
    let (mut key, mut time, mut signature, mut length) = (None, None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = Some(value.trim().to_string());
        // Only these headers are read; everything else, a proxy's address headers among them, is
        // dropped here unseen.
        match name.trim().to_ascii_lowercase().as_str() {
            "x-nitely-key" => key = value,
            "x-nitely-time" => time = value,
            "x-nitely-signature" => signature = value,
            "content-length" => length = value,
            "transfer-encoding" => return Err((411, "send the body with a Content-Length")),
            _ => {}
        }
    }
    let length: usize = match length {
        Some(v) => v.parse().map_err(|_| (400, "bad Content-Length"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err((413, "a report is at most 64 KiB"));
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        read_some(stream, &mut chunk, &mut body, deadline)?;
    }
    body.truncate(length);
    Ok(Request { method, path, key, time, signature, body })
}

fn read_some(stream: &mut TcpStream, chunk: &mut [u8], into: &mut Vec<u8>, deadline: Instant) -> std::result::Result<(), (u16, &'static str)> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
        return Err((408, "request timed out"));
    }
    match stream.read(chunk) {
        Ok(0) | Err(_) => Err((408, "request timed out")),
        Ok(n) => {
            into.extend_from_slice(&chunk[..n]);
            Ok(())
        }
    }
}

fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &str, public: bool) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let cache = if public { "public, max-age=300" } else { "no-store" };
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: {cache}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    // Read what the client still sends for a moment, so the close is clean and it gets the answer.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut sink = [0u8; 4096];
    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
}
