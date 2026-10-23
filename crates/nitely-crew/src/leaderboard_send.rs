//! Sharing on the Leaderboard (NIT-447): joining makes a signing key, the weekly send posts only
//! the report builder's output (`leaderboard_report`), the board is fetched to view it, and
//! leaving deletes what was sent. The service is `server/leaderboard` in this repo; its README
//! has the wire format.
//!
//! Nothing here touches the network until the person joined and sent once by hand, or asked to
//! view the board: every request goes through `Sharing::call`, and each caller checks first.
//!
//! The signing key is the install's only id. Its secret is a file in the data folder, not the OS
//! keychain, so the CLI and the app (and a test harness's data folder) each use the right one: it
//! can only send or delete this install's board entry.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::leaderboard_report::{Profile, Report};

/// Where reports go unless NITELY_LEADERBOARD_URL or the saved setting says otherwise.
pub const DEFAULT_URL: &str = "https://api.nitely.net";
const STATE_FILE: &str = "leaderboard.json";
/// Every delete, one JSON line each, kept after leaving too (NIT-653).
const DELETES_FILE: &str = "leaderboard-deletes.jsonl";
/// Report fields a service older than this client may not know: left out and sent again when it
/// refuses one of them, rather than not sending at all (NIT-653).
const NEWER_FIELDS: &[&str] = &["avatar", "closes_week"];
const KEY_FILE: &str = "leaderboard-key";
const WEEK: i64 = 7 * 86_400;
const DAY: i64 = 86_400;
/// The local hour on Sunday from which a send closes the week (NIT-629).
const SEND_HOUR: i64 = 18;

/// What joining saved.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct State {
    pub joined: bool,
    pub profile: Profile,
    /// The projects counted (never sent: only how many); empty counts every project.
    pub projects: Vec<String>,
    /// A service address other than the default.
    pub url: Option<String>,
    /// The ISO week of the last report the service took, and when.
    pub last_sent_week: Option<String>,
    pub last_sent_at: Option<i64>,
    pub last_error: Option<String>,
    /// The last time this install deleted what it sent from the board (NIT-653): when, how many
    /// reports, and from where (`app` or `cli`). A delete is never silent: the app shows this.
    pub last_deleted_at: Option<i64>,
    pub last_deleted: Option<u64>,
    pub last_deleted_by: Option<String>,
}

/// The sharing status, for the app and `nitely leaderboard status`.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub state: State,
    /// The public key, the install's id on the board; None before joining.
    pub id: Option<String>,
    pub url: String,
    /// Whether the day's send would go now.
    pub due: bool,
    /// When the next automatic send goes (now, when due); None before the first send by hand.
    pub next_send_at: Option<i64>,
    /// When this week's entry closes: its Sunday-evening send, after which it no longer changes.
    pub week_closes_at: i64,
}

/// This install's sharing, kept in `dir` (the data folder).
pub struct Sharing {
    dir: PathBuf,
}

impl Sharing {
    pub fn new(dir: &Path) -> Sharing {
        Sharing { dir: dir.to_path_buf() }
    }

    /// The person's own, in Nitely's data folder.
    pub fn default_dir() -> Sharing {
        Sharing::new(&nitely_core::paths::data_dir())
    }

    pub fn state(&self) -> Result<State> {
        match std::fs::read_to_string(self.dir.join(STATE_FILE)) {
            Ok(text) => serde_json::from_str(&text).context("reading leaderboard.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).context("reading leaderboard.json"),
        }
    }

    fn save(&self, state: &State) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{STATE_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
        std::fs::rename(&tmp, self.dir.join(STATE_FILE)).context("saving leaderboard.json")
    }

    /// The service address: NITELY_LEADERBOARD_URL, else the saved one, else the default.
    pub fn url(&self, state: &State) -> String {
        std::env::var("NITELY_LEADERBOARD_URL").ok().filter(|u| !u.trim().is_empty()).or_else(|| state.url.clone()).unwrap_or_else(|| DEFAULT_URL.to_string()).trim_end_matches('/').to_string()
    }

    pub fn status(&self, now: i64) -> Result<Status> {
        let state = self.state()?;
        let id = self.secret()?.map(|s| public_id(&s));
        let url = self.url(&state);
        let plan = plan(&state, now, crate::leaderboard::local_offset());
        Ok(Status { due: plan.due, next_send_at: plan.next_send_at, week_closes_at: plan.week_closes_at, state, id, url })
    }

    fn secret(&self) -> Result<Option<[u8; 32]>> {
        match std::fs::read_to_string(self.dir.join(KEY_FILE)) {
            Ok(hex) => {
                let bytes: Vec<u8> = (0..hex.trim().len()).step_by(2).filter_map(|i| hex.trim().get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
                Ok(Some(bytes.try_into().map_err(|_| anyhow::anyhow!("{KEY_FILE} is not a key Nitely made"))?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading the leaderboard key"),
        }
    }

    /// Joins, or changes what was chosen: saves the profile and projects, and makes the signing
    /// key the first time. Sends nothing; `profile` is checked the way the report checks it.
    pub fn join(&self, profile: &Profile, projects: &[String], url: Option<&str>) -> Result<Status> {
        crate::leaderboard_report::check_profile(profile)?;
        if let Some(u) = url {
            check_url(u)?;
        }
        if self.secret()?.is_none() {
            std::fs::create_dir_all(&self.dir)?;
            let secret = nitely_core::approval::new_secret()?;
            let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(self.dir.join(KEY_FILE), hex).context("saving the leaderboard key")?;
        }
        let mut state = self.state()?;
        state.joined = true;
        state.profile = profile.clone();
        state.projects = projects.to_vec();
        if url.is_some() {
            state.url = url.map(str::to_string);
        }
        self.save(&state)?;
        self.status(nitely_core::db::now())
    }

    /// Sends this week's report now (the first send is always by hand), replacing any sent
    /// earlier this week. One sent on Sunday evening closes the week.
    pub fn send(&self, config: &nitely_core::Config, db: &nitely_core::db::Db) -> Result<Report> {
        let state = self.state()?;
        if !state.joined {
            bail!("join the Leaderboard first: nothing is sent before you do");
        }
        let mut report = crate::leaderboard_report::report_now(config, db, &state.projects, &state.profile)?;
        report.closes_week = closes_week(nitely_core::db::now(), crate::leaderboard::local_offset());
        self.send_report(&report)?;
        Ok(report)
    }

    /// The day's send: sends only when joined, sent by hand once before, and `due`. Returns the
    /// week sent, if it sent.
    pub fn send_if_due(&self, config: &nitely_core::Config, db: &nitely_core::db::Db) -> Result<Option<String>> {
        let state = self.state()?;
        if !plan(&state, nitely_core::db::now(), crate::leaderboard::local_offset()).due {
            return Ok(None);
        }
        Ok(Some(self.send(config, db)?.week))
    }

    /// Posts `report`, signed; records the week on success and the error on failure.
    pub fn send_report(&self, report: &Report) -> Result<()> {
        let mut state = self.state()?;
        if !state.joined {
            bail!("join the Leaderboard first: nothing is sent before you do");
        }
        let mut body = serde_json::to_value(report)?;
        let result = loop {
            let result = self.call(&state, "POST", "/v1/leaderboard/reports", Some(&serde_json::to_vec(&body)?));
            // An older service refusing a field this client added: send the report without it.
            let refused = result.as_ref().err().and_then(|e| unknown_field(&format!("{e:#}")));
            match refused.filter(|f| NEWER_FIELDS.contains(&f.as_str()) && body.get(f).is_some()) {
                Some(field) => {
                    body.as_object_mut().map(|o| o.remove(&field));
                }
                None => break result,
            }
        };
        state.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if result.is_ok() {
            state.last_sent_week = Some(report.week.clone());
            state.last_sent_at = Some(nitely_core::db::now());
        }
        self.save(&state)?;
        result.map(|_| ())
    }

    /// Deletes everything this install sent from the board, and stays joined. Only ever on the
    /// person's word: `by` says where they asked (`app`, `cli`), and the delete is recorded in the
    /// state and in leaderboard-deletes.jsonl, so it is never silent (NIT-653).
    pub fn delete(&self, by: &str) -> Result<u64> {
        let state = self.state()?;
        if self.secret()?.is_none() {
            return Ok(0);
        }
        let answer = self.call(&state, "DELETE", "/v1/leaderboard/entries", Some(b""))?;
        let deleted = answer["deleted"].as_u64().unwrap_or(0);
        let now = nitely_core::db::now();
        let mut state = state;
        state.last_sent_week = None;
        state.last_sent_at = None;
        state.last_deleted_at = Some(now);
        state.last_deleted = Some(deleted);
        state.last_deleted_by = Some(by.to_string());
        self.save(&state)?;
        self.log_delete(now, deleted, by);
        Ok(deleted)
    }

    /// One line in leaderboard-deletes.jsonl. Best effort: the delete itself already happened.
    fn log_delete(&self, at: i64, deleted: u64, by: &str) {
        use std::io::Write;
        let line = serde_json::json!({ "at": at, "deleted": deleted, "by": by });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(DELETES_FILE)) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Leaves: deletes what was sent (unless it never sent anything), then forgets the profile and
    /// the key, so a later join is a new id. `by` as for `delete`.
    pub fn leave(&self, by: &str) -> Result<u64> {
        let state = self.state()?;
        let deleted = if state.last_sent_at.is_some() || state.last_sent_week.is_some() { self.delete(by)? } else { 0 };
        for file in [STATE_FILE, KEY_FILE] {
            match std::fs::remove_file(self.dir.join(file)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("removing {file}")),
                _ => {}
            }
        }
        Ok(deleted)
    }

    /// The public board for `period` (`week`, `30d` or `all`). Fetching it is a request to the
    /// service, so only the person asking to see it calls this.
    pub fn board(&self, period: &str) -> Result<Value> {
        if !["week", "30d", "all"].contains(&period) {
            bail!("a board is week, 30d or all");
        }
        self.call(&self.state()?, "GET", &format!("/v1/leaderboard/board/{period}.json"), None)
    }

    /// The one place a request leaves for the service: signed when it carries a body.
    fn call(&self, state: &State, method: &str, path: &str, body: Option<&[u8]>) -> Result<Value> {
        let url = self.url(state);
        check_url(&url)?;
        if std::env::var("NITELY_HARNESS").as_deref() == Ok("1") && !url.starts_with("http://") {
            bail!("the test harness reaches only a Leaderboard on this machine");
        }
        let agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(std::time::Duration::from_secs(30))).build().new_agent();
        let full = format!("{url}{path}");
        let mut res = match body {
            None => agent.get(&full).call(),
            Some(body) => {
                let secret = self.secret()?.context("join the Leaderboard first: there is no key")?;
                let key = public_id(&secret);
                // Milliseconds: each request must be later than the key's last one.
                let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
                let signature = URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&secret).sign(message(method, path, &key, time, body).as_bytes()).to_bytes());
                let req = ureq::http::Request::builder()
                    .method(method)
                    .uri(&full)
                    .header("Content-Type", "application/json")
                    .header("X-Nitely-Key", &key)
                    .header("X-Nitely-Time", time.to_string())
                    .header("X-Nitely-Signature", signature)
                    .body(body.to_vec())?;
                agent.run(req)
            }
        }
        .with_context(|| format!("reaching the Leaderboard at {url}"))?;
        let status = res.status().as_u16();
        let answer: Value = res.body_mut().read_json().unwrap_or(Value::Null);
        if status != 200 {
            bail!("the Leaderboard answered {status}: {}", answer["error"].as_str().unwrap_or("no reason given"));
        }
        Ok(answer)
    }
}

/// When the sends go (NIT-629).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Joined, sent by hand at least once, and a day since the last send, or Sunday 18:00 local
    /// passed since it.
    pub due: bool,
    /// When the next automatic send goes: now when due, else a day after the last, or Sunday
    /// 18:00 when that is sooner.
    pub next_send_at: Option<i64>,
    /// This week's Sunday at the send hour, when its entry closes; the next week's once the
    /// closing send went.
    pub week_closes_at: i64,
}

/// Today's send hour, local, as a time (`utc_offset` seconds east of UTC).
fn send_hour_of(now: i64, utc_offset: i64) -> i64 {
    let local = now + utc_offset;
    local - local.rem_euclid(DAY) + SEND_HOUR * 3600 - utc_offset
}

/// Whether a send at `now` is Sunday evening's, which closes its week.
pub fn closes_week(now: i64, utc_offset: i64) -> bool {
    use chrono::Datelike;
    let local = chrono::DateTime::from_timestamp(now + utc_offset, 0).unwrap_or_default();
    local.weekday() == chrono::Weekday::Sun && now >= send_hour_of(now, utc_offset)
}

/// When the sends go: once a day, at any hour, and at Sunday 18:00 local when that comes sooner, a
/// send then closing the week. Nothing goes before the first send by hand.
pub fn plan(state: &State, now: i64, utc_offset: i64) -> Plan {
    use chrono::Datelike;
    let today = send_hour_of(now, utc_offset);
    let local = chrono::DateTime::from_timestamp(now + utc_offset, 0).unwrap_or_default();
    let sunday = today + i64::from(6 - local.weekday().num_days_from_monday()) * DAY;
    let week_closes_at = if state.last_sent_at.is_some_and(|l| l >= sunday) { sunday + WEEK } else { sunday };
    let Some(last) = state.last_sent_at.filter(|_| state.joined) else {
        return Plan { due: false, next_send_at: None, week_closes_at };
    };
    let next = next_send(last, utc_offset);
    let due = now >= next;
    Plan { due, next_send_at: Some(if due { now } else { next }), week_closes_at }
}

/// When the send after one at `last` goes (NIT-634 review): a day later, at any hour the app is
/// running then, so someone who runs it only in the morning still shares daily; and at the next
/// Sunday 18:00 when that comes sooner, so the week closes with a send made then.
fn next_send(last: i64, utc_offset: i64) -> i64 {
    use chrono::Datelike;
    let local = chrono::DateTime::from_timestamp(last + utc_offset, 0).unwrap_or_default();
    let mut sunday = send_hour_of(last, utc_offset) + i64::from(6 - local.weekday().num_days_from_monday()) * DAY;
    if sunday <= last {
        sunday += WEEK;
    }
    (last + DAY).min(sunday)
}

/// The field an older service named in its refusal (`unknown field `avatar``), if any.
fn unknown_field(error: &str) -> Option<String> {
    let rest = error.split("unknown field `").nth(1)?;
    Some(rest.split('`').next()?.to_string())
}

/// The install's id: its public key, base64url without padding.
pub fn public_id(secret: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(secret).verifying_key().to_bytes())
}

/// The message a request's signature covers; the service checks the same one.
pub fn message(method: &str, path: &str, key: &str, time: i64, body: &[u8]) -> String {
    let digest: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    format!("nitely-leaderboard-v1\n{method}\n{path}\n{key}\n{time}\n{digest}")
}

/// https, or plain http to this machine only (a local service while developing).
fn check_url(url: &str) -> Result<()> {
    let local = ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"].iter().any(|p| url.starts_with(p));
    if !(url.starts_with("https://") || local) || url.chars().any(|c| c.is_whitespace()) {
        bail!("the Leaderboard address must be https (or http to 127.0.0.1 while developing): {url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONDAY: i64 = 1_791_763_200; // 2026-10-12 00:00 UTC

    /// NIT-629: after a first send by hand, a send goes once a day from 18:00 local, and Sunday
    /// evening's closes the week.
    #[test]
    fn the_send_goes_once_a_day_at_any_hour_and_sunday_evening_closes_the_week() {
        let h = 3600;
        let mut s = State { joined: true, ..Default::default() };
        let sunday_evening = MONDAY + 6 * DAY + 19 * h;
        let p = plan(&s, sunday_evening, 0);
        assert!(!p.due && p.next_send_at.is_none(), "never sent by hand: never automatic");
        // Sent by hand Monday 10:00: the next goes a day later, at whatever hour the app runs then.
        s.last_sent_at = Some(MONDAY + 10 * h);
        let p = plan(&s, MONDAY + 20 * h, 0);
        assert_eq!((p.due, p.next_send_at), (false, Some(MONDAY + DAY + 10 * h)), "once a day: tomorrow at 10:00");
        let p = plan(&s, MONDAY + DAY + 9 * h, 0);
        assert!(!p.due, "not a day yet");
        let p = plan(&s, MONDAY + DAY + 11 * h, 0);
        assert_eq!((p.due, p.next_send_at), (true, Some(MONDAY + DAY + 11 * h)), "a day since: now");
        // NIT-634 review: someone who runs the app only in the morning, weeks after the last send,
        // still sends.
        assert!(plan(&s, MONDAY + 3 * WEEK + 8 * h, 0).due);
        // Saturday 20:00's send: the next comes Sunday 18:00, sooner than a day, to close the week.
        s.last_sent_at = Some(MONDAY + 5 * DAY + 20 * h);
        let p = plan(&s, MONDAY + 6 * DAY + 12 * h, 0);
        assert_eq!((p.due, p.next_send_at), (false, Some(MONDAY + 6 * DAY + 18 * h)));
        assert!(plan(&s, sunday_evening, 0).due);
        assert_eq!(p.week_closes_at, MONDAY + 6 * DAY + 18 * h);
        assert!(closes_week(sunday_evening, 0) && !closes_week(MONDAY + 6 * DAY + 17 * h, 0) && !closes_week(MONDAY + 20 * h, 0));
        s.last_sent_at = Some(sunday_evening);
        assert_eq!(plan(&s, sunday_evening + h, 0).week_closes_at, MONDAY + 13 * DAY + 18 * h, "closed: the next week's");
        assert_eq!(plan(&s, sunday_evening + h, 0).next_send_at, Some(sunday_evening + DAY), "and a day later after the closing send");
        // Local time: at UTC-2, 19:00 UTC on Sunday is 17:00 there, before the week closes.
        assert!(!closes_week(sunday_evening, -2 * h));
        s.last_sent_at = Some(MONDAY + 5 * DAY + 20 * h);
        assert_eq!(plan(&s, MONDAY + 6 * DAY + 12 * h, -2 * h).next_send_at, Some(MONDAY + 6 * DAY + 20 * h), "Sunday 18:00 there is 20:00 UTC");
        s.joined = false;
        assert!(!plan(&s, MONDAY + 3 * WEEK, 0).due);
    }

    #[test]
    fn nothing_goes_out_before_joining() {
        let dir = nitely_core::paths::unique_temp("nitely-leaderboard-send", "");
        let sharing = Sharing::new(&dir);
        let report = crate::leaderboard_report::Report {
            schema: 1,
            display_name: "x".into(),
            links: Default::default(),
            avatar: None,
            week: "2026-W42".into(),
            projects: 0,
            periods: vec![],
            weekly: vec![],
            records: Default::default(),
            closes_week: false,
        };
        let err = sharing.send_report(&report).unwrap_err().to_string();
        assert!(err.contains("join the Leaderboard first"), "{err}");
        assert_eq!(sharing.delete("test").unwrap(), 0, "no key: nothing to delete, nothing asked");
        assert_eq!(sharing.leave("test").unwrap(), 0);
        assert!(!dir.join(KEY_FILE).exists() && !dir.join(STATE_FILE).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A stand-in for the service on this machine that counts every connection made to it and
    /// answers each with `body`.
    struct CountingService {
        url: String,
        connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl CountingService {
        fn start(body: &'static str) -> CountingService {
            use std::io::{Read, Write};
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (count, done) = (connections.clone(), stop.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut conn, _)) => {
                            count.fetch_add(1, Ordering::SeqCst);
                            conn.set_nonblocking(false).ok();
                            conn.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
                            let mut buf = [0u8; 4096];
                            let _ = conn.read(&mut buf);
                            let reply = format!("HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: {}
Connection: close

{body}", body.len());
                            let _ = conn.write_all(reply.as_bytes());
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    }
                }
            });
            CountingService { url, connections, stop }
        }

        fn connections(&self) -> usize {
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Drop for CountingService {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A stand-in for a service older than this client: it refuses a report naming `avatar`, as
    /// one built before NIT-630 did, takes any other, and answers a delete with two reports. It
    /// keeps every request body it was sent.
    fn older_service() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let bodies = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                conn.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
                // Headers, then as much body as Content-Length says.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, mut body) = loop {
                    let n = conn.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (String::new(), Vec::new());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (String::from_utf8_lossy(&buf[..i]).to_string(), buf[i + 4..].to_vec());
                    }
                };
                let length: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                while body.len() < length {
                    let n = conn.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&body).to_string();
                bodies.lock().unwrap().push(body.clone());
                let (status, answer) = if head.starts_with("DELETE") {
                    ("200 OK", r#"{"deleted":2}"#.to_string())
                } else if body.contains("\"avatar\"") {
                    ("400 Bad Request", r#"{"error":"not a schema 1 report: unknown field `avatar`, expected one of `schema`, `display_name`"}"#.to_string())
                } else {
                    ("200 OK", r#"{"ok":true,"week":"2026-W43"}"#.to_string())
                };
                let _ = write!(conn, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
            }
        });
        (url, seen)
    }

    /// NIT-653: a client newer than its service keeps its place: the report goes again without the
    /// field the service does not know. And deleting is the person's act, recorded where they see
    /// it: in the state the app shows, and in a log that outlives leaving.
    #[test]
    fn a_newer_client_still_sends_and_a_delete_is_never_silent() {
        let (url, seen) = older_service();
        let dir = nitely_core::paths::unique_temp("nitely-leaderboard-newer", "");
        let sharing = Sharing::new(&dir);
        let profile = Profile { display_name: "Night Owl".into(), github: Some("octocat".into()), x: None, website: None, avatar: Some("github".into()) };
        sharing.join(&profile, &[], Some(&url)).unwrap();
        let mut report = crate::leaderboard_report::Report {
            schema: 1,
            display_name: "Night Owl".into(),
            links: crate::leaderboard_report::Links { github: Some("https://github.com/octocat".into()), ..Default::default() },
            avatar: Some("github".into()),
            week: "2026-W43".into(),
            projects: 1,
            periods: vec![],
            weekly: vec![],
            records: Default::default(),
            closes_week: false,
        };
        sharing.send_report(&report).unwrap();
        let state = sharing.state().unwrap();
        assert_eq!((state.last_sent_week.as_deref(), state.last_error.as_deref()), (Some("2026-W43"), None), "sent, with nothing left failing");
        let bodies = seen.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "refused once, then sent without the field");
        assert!(bodies[0].contains("\"avatar\"") && !bodies[1].contains("\"avatar\"") && bodies[1].contains("Night Owl"));

        // A refusal the client cannot answer by leaving out a newer field stays an error, and a
        // failed send forgets nothing that was sent before.
        report.avatar = None;
        report.display_name = "avatar".into();
        assert!(sharing.send_report(&report).is_err());
        let state = sharing.state().unwrap();
        assert!(state.last_error.is_some() && state.last_sent_week.as_deref() == Some("2026-W43"), "{state:?}");

        assert_eq!(sharing.delete("app").unwrap(), 2);
        let state = sharing.state().unwrap();
        assert_eq!((state.last_sent_week, state.last_deleted, state.last_deleted_by.as_deref()), (None, Some(2), Some("app")));
        assert!(state.last_deleted_at.is_some());
        sharing.leave("cli").unwrap();
        let log = std::fs::read_to_string(dir.join(DELETES_FILE)).unwrap();
        let lines: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.iter().map(|l| (l["deleted"].as_u64(), l["by"].as_str().map(str::to_string))).collect::<Vec<_>>(), [(Some(2), Some("app".to_string()))], "leaving after the delete had nothing more to delete, and the log outlives it");
        std::fs::remove_dir_all(dir).ok();
    }

    /// NIT-449: the leaderboard reaches its service only when the person asks it to. Reading the
    /// status, joining, saving changes, the weekly check before a first send by hand, and leaving
    /// without having sent all stay on this machine; viewing the board is the one request a
    /// person can make before sharing, and it is one request.
    #[test]
    fn nothing_reaches_the_service_before_the_person_asks() {
        let service = CountingService::start(r#"{"schema":1,"period":"week","week":"2026-W43","generated_at":0,"entries":[]}"#);
        let dir = nitely_core::paths::unique_temp("nitely-leaderboard-optin", "");
        let sharing = Sharing::new(&dir);
        // The address is saved only by joining; before that, point the state at the stand-in so a
        // stray request would be seen rather than go to the real service.
        sharing.save(&State { url: Some(service.url.clone()), ..Default::default() }).unwrap();
        let db = nitely_core::db::Db::open(&dir.join("t.db")).unwrap();
        let config = nitely_core::Config::default();
        let report = crate::leaderboard_report::Report {
            schema: 1,
            display_name: "x".into(),
            links: Default::default(),
            avatar: None,
            week: "2026-W43".into(),
            projects: 0,
            periods: vec![],
            weekly: vec![],
            records: Default::default(),
            closes_week: false,
        };

        // Not joined: nothing goes anywhere, whatever is asked.
        assert!(!sharing.status(1_792_000_000).unwrap().state.joined);
        assert_eq!(sharing.send_if_due(&config, &db).unwrap(), None);
        assert!(sharing.send(&config, &db).is_err(), "sending before joining is refused");
        assert!(sharing.send_report(&report).is_err());
        assert_eq!(sharing.delete("test").unwrap(), 0);
        assert_eq!(service.connections(), 0, "before joining");

        // Joining and changing the profile: still nothing, and no weekly send before one by hand.
        let profile = Profile { display_name: "Night Owl".into(), github: Some("octocat".into()), x: None, website: None, avatar: None };
        sharing.join(&profile, &[], Some(&service.url)).unwrap();
        sharing.join(&Profile { display_name: "Owl".into(), ..profile.clone() }, &["NIT".into()], None).unwrap();
        assert!(sharing.status(1_792_000_000).unwrap().state.joined);
        assert_eq!(sharing.send_if_due(&config, &db).unwrap(), None, "never automatic before a send by hand");
        assert_eq!(service.connections(), 0, "joining sends nothing");

        // Viewing the board is a request the person makes: exactly one.
        let board = sharing.board("week").unwrap();
        assert_eq!(board["entries"], serde_json::json!([]));
        assert_eq!(service.connections(), 1, "viewing the board is one request");

        // Leaving without ever sending deletes nothing remotely, so it asks nothing.
        assert_eq!(sharing.leave("test").unwrap(), 0);
        assert_eq!(service.connections(), 1, "leaving unsent stays on this machine");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn addresses_are_https_or_this_machine() {
        assert!(check_url("https://example.com").is_ok());
        assert!(check_url("http://127.0.0.1:8787").is_ok());
        assert!(check_url("http://example.com").is_err());
        assert!(check_url("http://127.0.0.1.example.com").is_err());
    }
}
