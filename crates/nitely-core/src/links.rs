//! Issue links that open from anywhere (NIT-228). `nitely://issue/NIT-12` opens the app only where
//! custom schemes are followed: the OS, a terminal. Chats, GitHub and most apps open only web links,
//! so the app also answers `http://127.0.0.1:7423/i/NIT-12` on loopback: the same thing, focusing
//! the app on that issue. It only opens a view, never changes anything, so it needs no token.
//!
//! The port is fixed, so a link pasted into a chat or a pull request today still opens the issue
//! next month: links never name another port. When something else holds it, the app says so and
//! takes it as soon as it is free (`listen`).

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// The loopback port issue links use: beside the relay's 7421 and the phone's 7422.
pub const PORT: u16 = 7423;
/// The path an issue's key follows.
pub const PATH: &str = "/i/";

/// The web link to an issue: `http://127.0.0.1:7423/i/NIT-12`.
pub fn issue_url(key: &str) -> String {
    format!("http://127.0.0.1:{PORT}{PATH}{}", key.trim().to_uppercase())
}

/// The issue key a link names, in either form: `nitely://issue/NIT-12` or a loopback
/// `http://127.0.0.1:<port>/i/NIT-12` (`localhost` too), with or without a trailing slash.
pub fn key_from_url(link: &str) -> Option<String> {
    let link = link.trim();
    let rest = if let Some(rest) = link.strip_prefix(&format!("{}://issue/", crate::issues::LINK_SCHEME)) {
        rest
    } else {
        let after = link.strip_prefix("http://")?;
        let (host, path) = after.split_once('/')?;
        let (name, port) = host.rsplit_once(':')?;
        if !matches!(name, "127.0.0.1" | "localhost") || port.parse::<u16>().is_err() {
            return None;
        }
        path.strip_prefix(PATH.trim_start_matches('/'))?
    };
    valid_key(rest.trim_end_matches('/'))
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

/// Answers issue links for the life of the process: takes `PORT`, and while something else holds
/// it, says so once through `taken` and tries again every 30 seconds.
pub fn listen(open: impl Fn(String) + Send + Sync + 'static, taken: impl Fn(&anyhow::Error)) {
    let mut told = false;
    loop {
        match bind() {
            Ok(listener) => return serve(listener, open),
            Err(e) => {
                if !told {
                    taken(&e);
                    told = true;
                }
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    }
}

/// Answers issue links on `listener` until the process ends, calling `open` with each key a link
/// names. One short-lived thread per request.
pub fn serve(listener: TcpListener, open: impl Fn(String) + Send + Sync + 'static) {
    let open = std::sync::Arc::new(open);
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(PORT);
    for stream in listener.incoming().flatten() {
        let open = open.clone();
        std::thread::spawn(move || {
            answer(stream, port, |k| open(k)).ok();
        });
    }
}

/// Reads one request and answers it: a page saying the issue opened, after `open` was called with
/// its key; 404 for anything else. Only GET and HEAD, only for a loopback host name.
fn answer(mut stream: TcpStream, port: u16, open: impl Fn(String)) -> Result<()> {
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
    let (status, page) = respond(&head, port, open);
    let body = if head.starts_with("HEAD ") { String::new() } else { page };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

/// The status line and page for a request's head; calls `open` for an issue link.
fn respond(head: &str, port: u16, open: impl Fn(String)) -> (&'static str, String) {
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
    let Some(key) = target.strip_prefix(PATH).map(|k| k.split(['?', '#']).next().unwrap_or("").trim_end_matches('/')).and_then(valid_key) else {
        return ("404 Not Found", page("Nitely", "This is not an issue link. They look like /i/NIT-12."));
    };
    if method == "GET" {
        open(key.clone());
    }
    let app = crate::issues::issue_link(&key);
    (
        "200 OK",
        page(
            &format!("{key} · Nitely"),
            &format!("Opened <b>{key}</b> in Nitely. You can close this tab. <span class=\"m\">Nothing came up? Open it with <a href=\"{app}\">the app's own link</a>.</span>"),
        ),
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
    }

    #[test]
    fn answers_an_issue_link_by_opening_it_and_nothing_else() {
        let opened = std::cell::RefCell::new(Vec::new());
        let ask = |head: &str| respond(head, 7423, |k| opened.borrow_mut().push(k));
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
    }

    #[test]
    fn a_running_listener_answers_over_loopback() {
        // A port of its own, so a running app on 7423 is never touched.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        std::thread::spawn(move || serve(listener, move |k| tx.lock().unwrap().send(k).unwrap()));
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /i/APP-3 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK") && reply.contains("Opened <b>APP-3</b>"), "{reply}");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "APP-3");
    }
}
