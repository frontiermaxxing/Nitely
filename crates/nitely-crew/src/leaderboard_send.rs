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
const KEY_FILE: &str = "leaderboard-key";
const WEEK: i64 = 7 * 86_400;
const DAY: i64 = 86_400;
/// The local hour from which the day's send goes (NIT-629): the first time the app runs after it.
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
        let body = serde_json::to_vec(report)?;
        let result = self.call(&state, "POST", "/v1/leaderboard/reports", Some(&body));
        state.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
        if result.is_ok() {
            state.last_sent_week = Some(report.week.clone());
            state.last_sent_at = Some(nitely_core::db::now());
        }
        self.save(&state)?;
        result.map(|_| ())
    }

    /// Deletes everything this install sent from the board, and stays joined.
    pub fn delete(&self) -> Result<u64> {
        let state = self.state()?;
        if self.secret()?.is_none() {
            return Ok(0);
        }
        let answer = self.call(&state, "DELETE", "/v1/leaderboard/entries", Some(b""))?;
        let mut state = state;
        state.last_sent_week = None;
        state.last_sent_at = None;
        self.save(&state)?;
        Ok(answer["deleted"].as_u64().unwrap_or(0))
    }

    /// Leaves: deletes what was sent (unless it never sent anything), then forgets the profile and
    /// the key, so a later join is a new id.
    pub fn leave(&self) -> Result<u64> {
        let state = self.state()?;
        let deleted = if state.last_sent_at.is_some() || state.last_sent_week.is_some() { self.delete()? } else { 0 };
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
    /// Joined, sent by hand at least once, past today's send hour (18:00 local), and nothing sent
    /// since it.
    pub due: bool,
    /// When the next automatic send goes: now when due, else today's or tomorrow's send hour.
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

/// When the sends go: once a day, the first time the app runs after the send hour, and Sunday
/// evening's closes the week. Nothing goes before the first send by hand.
pub fn plan(state: &State, now: i64, utc_offset: i64) -> Plan {
    use chrono::Datelike;
    let today = send_hour_of(now, utc_offset);
    let local = chrono::DateTime::from_timestamp(now + utc_offset, 0).unwrap_or_default();
    let sunday = today + i64::from(6 - local.weekday().num_days_from_monday()) * DAY;
    let week_closes_at = if state.last_sent_at.is_some_and(|l| l >= sunday) { sunday + WEEK } else { sunday };
    let Some(last) = state.last_sent_at.filter(|_| state.joined) else {
        return Plan { due: false, next_send_at: None, week_closes_at };
    };
    let due = now >= today && last < today;
    let next_send_at = Some(if due { now } else if now < today { today } else { today + DAY });
    Plan { due, next_send_at, week_closes_at }
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
    fn the_send_goes_once_a_day_after_the_send_hour_and_sunday_evening_closes_the_week() {
        let h = 3600;
        let mut s = State { joined: true, ..Default::default() };
        let sunday_evening = MONDAY + 6 * DAY + 19 * h;
        let p = plan(&s, sunday_evening, 0);
        assert!(!p.due && p.next_send_at.is_none(), "never sent by hand: never automatic");
        // Sent by hand Monday 10:00.
        s.last_sent_at = Some(MONDAY + 10 * h);
        let p = plan(&s, MONDAY + 12 * h, 0);
        assert_eq!((p.due, p.next_send_at), (false, Some(MONDAY + 18 * h)), "before the send hour: tonight");
        let p = plan(&s, MONDAY + 20 * h, 0);
        assert_eq!((p.due, p.next_send_at), (true, Some(MONDAY + 20 * h)), "after it, nothing sent since: now");
        s.last_sent_at = Some(MONDAY + 20 * h);
        let p = plan(&s, MONDAY + 22 * h, 0);
        assert_eq!((p.due, p.next_send_at), (false, Some(MONDAY + DAY + 18 * h)), "once a day: tomorrow");
        assert!(plan(&s, MONDAY + DAY + 9 * h, 0).next_send_at == Some(MONDAY + DAY + 18 * h) && !plan(&s, MONDAY + DAY + 9 * h, 0).due);
        assert!(plan(&s, MONDAY + 3 * DAY + 18 * h, 0).due, "a day the app did not run is skipped, not made up");
        // The week closes Sunday 18:00, and the send then is the closing one.
        assert_eq!(plan(&s, MONDAY + 22 * h, 0).week_closes_at, MONDAY + 6 * DAY + 18 * h);
        assert!(closes_week(sunday_evening, 0) && !closes_week(MONDAY + 6 * DAY + 17 * h, 0) && !closes_week(MONDAY + 20 * h, 0));
        s.last_sent_at = Some(sunday_evening);
        assert_eq!(plan(&s, sunday_evening + h, 0).week_closes_at, MONDAY + 13 * DAY + 18 * h, "closed: the next week's");
        // Local time: at UTC-2 it is 17:00 there at 19:00 UTC.
        s.last_sent_at = Some(MONDAY + 10 * h);
        assert!(!plan(&s, MONDAY + 19 * h, -2 * h).due && plan(&s, MONDAY + 20 * h, -2 * h).due);
        assert!(!closes_week(sunday_evening, -2 * h));
        s.joined = false;
        assert!(!plan(&s, MONDAY + 20 * h, 0).due);
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
        assert_eq!(sharing.delete().unwrap(), 0, "no key: nothing to delete, nothing asked");
        assert_eq!(sharing.leave().unwrap(), 0);
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
        assert_eq!(sharing.delete().unwrap(), 0);
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
        assert_eq!(sharing.leave().unwrap(), 0);
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
