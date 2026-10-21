//! Issue links that open from anywhere (NIT-228). `nitely://issue/NIT-12` opens the app only where
//! custom schemes are followed: the OS, a terminal. Chats, GitHub and most apps open only web links,
//! so the app also answers `http://127.0.0.1:7423/i/NIT-12` on loopback: the same thing, focusing
//! the app on that issue. It only opens a view, never changes anything, so it needs no token.
//!
//! The port is fixed, so a link pasted into a chat or a pull request today still opens the issue
//! next month: links never name another port.
//!
//! Two processes can answer: the app while it runs, and `nitely links serve`, a small listener
//! started at login (link_service.rs) that forwards a link to the app, starting it first when it is
//! closed. When the listener is registered the app leaves the port to it; otherwise the app takes it.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// The loopback port issue links use: beside the relay's 7421 and the phone's 7422.
pub const PORT: u16 = 7423;
/// The path an issue's key follows.
pub const PATH: &str = "/i/";
/// The path an ask's number follows (NIT-546).
pub const ASK_PATH: &str = "/a/";

/// What a link opens: an issue, or an ask with its question, answer and issue (NIT-546).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Issue(String),
    Ask(i64),
}

impl Target {
    /// The app's own link: `nitely://issue/NIT-12` or `nitely://ask/69`.
    pub fn app_link(&self) -> String {
        match self {
            Target::Issue(key) => crate::issues::issue_link(key),
            Target::Ask(id) => format!("{}://ask/{id}", crate::issues::LINK_SCHEME),
        }
    }

    /// What the app's page is told to open: the issue's key, or `ask:69`.
    pub fn token(&self) -> String {
        match self {
            Target::Issue(key) => key.clone(),
            Target::Ask(id) => format!("ask:{id}"),
        }
    }

    /// How a page names it: `NIT-12`, `ask 69`.
    fn label(&self) -> String {
        match self {
            Target::Issue(key) => key.clone(),
            Target::Ask(id) => format!("ask {id}"),
        }
    }
}

/// The web link to an ask: `http://127.0.0.1:7423/a/69` (NIT-546).
pub fn ask_url(id: i64) -> String {
    format!("http://127.0.0.1:{PORT}{ASK_PATH}{id}")
}
/// What a listener answers on `/health`, followed by its role: how one finds another.
const HEALTH: &str = "nitely-links";

/// Which process answers: the app itself, or the listener started at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    App,
    Service,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::App => "app",
            Role::Service => "service",
        }
    }
}

/// The web link to an issue: `http://127.0.0.1:7423/i/NIT-12`.
pub fn issue_url(key: &str) -> String {
    format!("http://127.0.0.1:{PORT}{PATH}{}", key.trim().to_uppercase())
}

/// The issue key a link names, in either form: `nitely://issue/NIT-12` or a loopback
/// `http://127.0.0.1:<port>/i/NIT-12` (`localhost` too), with or without a trailing slash.
pub fn key_from_url(link: &str) -> Option<String> {
    match target_from_url(link)? {
        Target::Issue(key) => Some(key),
        Target::Ask(_) => None,
    }
}

/// What a link opens, in either form: `nitely://issue/NIT-12`, `nitely://ask/69`, or the loopback
/// web forms `http://127.0.0.1:<port>/i/NIT-12` and `/a/69` (`localhost` too).
pub fn target_from_url(link: &str) -> Option<Target> {
    let link = link.trim();
    let scheme = format!("{}://", crate::issues::LINK_SCHEME);
    let (issue, ask) = if let Some(rest) = link.strip_prefix(&scheme) {
        (rest.strip_prefix("issue/"), rest.strip_prefix("ask/"))
    } else {
        let after = link.strip_prefix("http://")?;
        let (host, path) = after.split_once('/')?;
        let (name, port) = host.rsplit_once(':')?;
        if !matches!(name, "127.0.0.1" | "localhost") || port.parse::<u16>().is_err() {
            return None;
        }
        (path.strip_prefix(PATH.trim_start_matches('/')), path.strip_prefix(ASK_PATH.trim_start_matches('/')))
    };
    match (issue, ask) {
        (Some(key), _) => valid_key(key.trim_end_matches('/')).map(Target::Issue),
        (_, Some(id)) => valid_ask(id.trim_end_matches('/')).map(Target::Ask),
        _ => None,
    }
}

/// An ask's number: digits only, not too many.
fn valid_ask(s: &str) -> Option<i64> {
    (!s.is_empty() && s.len() <= 12 && s.chars().all(|c| c.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// `NIT-12`, upper-cased, when `s` is a project key, a dash and a number; nothing else.
fn valid_key(s: &str) -> Option<String> {
    let (project, number) = s.split_once('-')?;
    let ok = !project.is_empty()
        && project.len() <= 10
        && project.starts_with(|c: char| c.is_ascii_alphabetic())
        && project.chars().all(|c| c.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.len() <= 9
        && number.chars().all(|c| c.is_ascii_digit());
    ok.then(|| s.to_uppercase())
}

/// Listens on loopback at `PORT`.
pub fn bind() -> Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, PORT))).with_context(|| format!("127.0.0.1:{PORT} is taken, so issue links cannot open the app until it is free"))
}

/// Who answers on `PORT` now: `Some(Role)` for a Nitely listener, None when nothing does (or
/// something that is not Nitely holds it).
pub fn probe() -> Option<Role> {
    probe_port(PORT)
}

fn probe_port(port: u16) -> Option<Role> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut reply = String::new();
    s.read_to_string(&mut reply).ok()?;
    let body = reply.split("\r\n\r\n").nth(1)?.trim();
    match body.strip_prefix(HEALTH)?.trim() {
        "app" => Some(Role::App),
        "service" => Some(Role::Service),
        _ => None,
    }
}

/// Answers issue and ask links on `listener` as `role` while `keep` says so (asked about once a
/// second), calling `open` with what each link names. One short-lived thread per request.
pub fn serve(listener: TcpListener, role: Role, open: impl Fn(Target) + Send + Sync + 'static, keep: impl Fn() -> bool) {
    let open = std::sync::Arc::new(open);
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(PORT);
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let mut asked = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).ok();
                let open = open.clone();
                std::thread::spawn(move || {
                    answer(stream, port, role, |k| open(k)).ok();
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
        if asked.elapsed() >= Duration::from_secs(1) {
            if !keep() {
                return;
            }
            asked = Instant::now();
        }
    }
}

/// Reads one request and answers it (`respond`). Only GET and HEAD, only for a loopback host name.
fn answer(mut stream: TcpStream, port: u16, role: Role, open: impl Fn(Target)) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let (status, page) = respond(&head, port, role, open);
    let body = if head.starts_with("HEAD ") { String::new() } else { page };
    let kind = if body.starts_with(HEALTH) { "text/plain" } else { "text/html" };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

/// The status line and page for a request's head; calls `open` for an issue link. `/health` says
/// which role answers, for `probe`.
fn respond(head: &str, port: u16, role: Role, open: impl Fn(Target)) -> (&'static str, String) {
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if !matches!(method, "GET" | "HEAD") {
        return ("405 Method Not Allowed", page("Nitely", "Issue links only open a view."));
    }
    // Only a loopback name: a page served from elsewhere that rebinds its own domain to this
    // address is not answered.
    let host = lines.find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("host")).map(|(_, v)| v.trim().to_string()));
    let loopback = host.as_deref().is_some_and(|h| [format!("127.0.0.1:{port}"), format!("localhost:{port}")].iter().any(|ok| h.eq_ignore_ascii_case(ok)));
    if !loopback {
        return ("421 Misdirected Request", page("Nitely", "Issue links answer on 127.0.0.1 only."));
    }
    if target == "/health" {
        return ("200 OK", format!("{HEALTH} {}", role.name()));
    }
    let path = target.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
    let opened = path.strip_prefix(PATH).and_then(valid_key).map(Target::Issue).or_else(|| path.strip_prefix(ASK_PATH).and_then(valid_ask).map(Target::Ask));
    let Some(opened) = opened else {
        return ("404 Not Found", page("Nitely", "This is not an issue or ask link. They look like /i/NIT-12 and /a/69."));
    };
    if method == "GET" {
        open(opened.clone());
    }
    let (app, label) = (opened.app_link(), opened.label());
    let done = match role {
        Role::App => format!("Opened <b>{label}</b> in Nitely. You can close this tab."),
        Role::Service => format!("Opening <b>{label}</b> in Nitely… You can close this tab."),
    };
    (
        "200 OK",
        page(&format!("{label} · Nitely"), &format!("{done} <span class=\"m\">Nothing came up? Open it with <a href=\"{app}\">the app's own link</a>.</span>")),
    )
}

fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title>\
<style>body{{font:15px/1.5 system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100vh;background:#f7f6f3;color:#1d1c1a}}\
p{{max-width:30em;margin:0 16px}}.m{{display:block;margin-top:8px;color:#6b6862;font-size:13px}}a{{color:inherit}}\
@media (prefers-color-scheme:dark){{body{{background:#171614;color:#ecebe8}}.m{{color:#9a978f}}}}</style><p>{text}</p>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn reads_either_form_of_link() {
        for link in ["nitely://issue/NIT-14", "nitely://issue/nit-14/", "http://127.0.0.1:7423/i/NIT-14", "http://localhost:7425/i/nit-14/"] {
            assert_eq!(key_from_url(link).as_deref(), Some("NIT-14"), "{link}");
        }
        for link in ["http://example.com:7423/i/NIT-14", "http://127.0.0.1/i/NIT-14", "https://127.0.0.1:7423/i/NIT-14", "http://127.0.0.1:7423/x/NIT-14", "http://127.0.0.1:7423/i/NIT-", "nitely://project/NIT"] {
            assert_eq!(key_from_url(link), None, "{link}");
        }
        // Asks have links of their own (NIT-546).
        for link in ["nitely://ask/69", "http://127.0.0.1:7423/a/69", "http://localhost:7423/a/69/"] {
            assert_eq!(target_from_url(link), Some(Target::Ask(69)), "{link}");
            assert_eq!(key_from_url(link), None, "an ask is no issue: {link}");
        }
        for link in ["http://127.0.0.1:7423/a/", "http://127.0.0.1:7423/a/x9", "nitely://ask/-1", "http://127.0.0.1:7423/a/1234567890123"] {
            assert_eq!(target_from_url(link), None, "{link}");
        }
        assert_eq!(ask_url(69), "http://127.0.0.1:7423/a/69");
        assert_eq!((Target::Ask(69).app_link(), Target::Ask(69).token()), ("nitely://ask/69".to_string(), "ask:69".to_string()));
        assert_eq!((Target::Issue("NIT-3".into()).app_link(), Target::Issue("NIT-3".into()).token()), ("nitely://issue/NIT-3".to_string(), "NIT-3".to_string()));
    }

    #[test]
    fn answers_an_issue_link_by_opening_it_and_nothing_else() {
        let opened = std::cell::RefCell::new(Vec::new());
        let ask = |head: &str| respond(head, 7423, Role::App, |t| opened.borrow_mut().push(t.token()));
        let (status, page) = ask("GET /i/nit-228 HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n");
        assert_eq!(status, "200 OK");
        assert!(page.contains("Opened <b>NIT-228</b>") && page.contains("nitely://issue/NIT-228"), "{page}");
        assert_eq!(ask("GET /i/NIT-9?from=pr HTTP/1.1\r\nhost: localhost:7423\r\n\r\n").0, "200 OK");
        assert_eq!(ask("HEAD /i/NIT-5 HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n").0, "200 OK");
        assert_eq!(opened.borrow().as_slice(), ["NIT-228", "NIT-9"], "HEAD looks without opening");
        // Nothing else opens anything.
        assert_eq!(ask("POST /i/NIT-1 HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n").0, "405 Method Not Allowed");
        assert_eq!(ask("GET /i/NIT-1 HTTP/1.1\r\nHost: evil.example:7423\r\n\r\n").0, "421 Misdirected Request");
        assert_eq!(ask("GET /i/NIT-1 HTTP/1.1\r\n\r\n").0, "421 Misdirected Request", "no host, no answer");
        assert_eq!(ask("GET /i/<script> HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n").0, "404 Not Found");
        assert_eq!(ask("GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n").0, "404 Not Found");
        assert_eq!(opened.borrow().len(), 2);
        // The listener started at login says it is opening the app, which may take a moment.
        let (_, page) = respond("GET /i/NIT-7 HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n", 7423, Role::Service, |_| {});
        assert!(page.contains("Opening <b>NIT-7</b>"), "{page}");
        assert_eq!(respond("GET /health HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n", 7423, Role::Service, |_| {}).1, "nitely-links service");
        assert_eq!(opened.borrow().len(), 2, "health opens nothing");
        // An ask's link opens the ask (NIT-546).
        let (status, page) = ask("GET /a/69 HTTP/1.1\r\nHost: 127.0.0.1:7423\r\n\r\n");
        assert_eq!(status, "200 OK");
        assert!(page.contains("Opened <b>ask 69</b>") && page.contains("nitely://ask/69"), "{page}");
        assert_eq!(opened.borrow().last().map(String::as_str), Some("ask:69"));
    }

    #[test]
    fn a_running_listener_answers_over_loopback() {
        // A port of its own, so a running app on 7423 is never touched.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let served = std::thread::spawn(move || serve(listener, Role::App, move |t| tx.lock().unwrap().send(t.token()).unwrap(), move || !stopping.load(std::sync::atomic::Ordering::SeqCst)));
        assert_eq!(probe_port(port), Some(Role::App), "a listener finds another by its health");
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /i/APP-3 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK") && reply.contains("Opened <b>APP-3</b>"), "{reply}");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "APP-3");
        // Asked to stop (the listener at login took over), it closes its listener: nothing answers
        // on the port. Binding it again at once is not the promise: macOS refuses that while one of
        // the port's own connections is still closing, and whoever takes over retries its bind.
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        served.join().unwrap();
        let refused = TcpStream::connect(("127.0.0.1", port)).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused, "the listener is closed");
    }
}
