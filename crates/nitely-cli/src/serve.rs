//! `nitely serve`: this machine's backlog for agents on others on the local network. It is the
//! MCP server (`mcp.rs`) over Streamable HTTP at `/mcp`, so Claude Code elsewhere adds it with
//! `claude mcp add --transport http` and needs no Nitely install. Every request must come from
//! this machine's networks and carry a token, made in the app and shown once there; the token's name is the agent's
//! identity. The server announces itself over mDNS as `_nitely._tcp`, which `nitely discover` and
//! macOS's `dns-sd -B _nitely._tcp` find.
//!
//! The HTTP layer is deliberately small and strict: one thread per connection with a cap, read and
//! write timeouts, a bounded header and body, Content-Length only, and the connection closed after
//! each answer. A slow or hostile client ties up its own thread for seconds, never the server.

use crate::mcp;
use anyhow::{Context, Result};
use nitely_core::relay::{self, lan_addresses, networks, url, DEFAULT_PORT, MCP_PATH, SERVICE_TYPE};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Larger bodies are refused unread; a tool call is a few kilobytes.
const MAX_BODY: usize = 1 << 20;
/// The request line and headers together.
const MAX_HEAD: usize = 16 << 10;
/// Connections handled at once; past this, new ones get 503 at once.
const MAX_CONNECTIONS: usize = 32;
/// How long a connection may take to send its whole request, however it trickles in, and to read
/// the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections one address may hold at once, so one machine cannot take every slot.
const MAX_PER_ADDRESS: usize = 4;
/// Open sessions kept; the oldest is dropped past this, and its client starts a new one.
const MAX_SESSIONS: usize = 256;
/// After answering, how long and how much of what the client still sends is read and dropped
/// before the connection closes. Closing with unread data makes Windows reset the connection,
/// and the client can lose the answer it was about to read (NIT-120).
const LINGER: Duration = Duration::from_secs(2);
const MAX_DRAIN: usize = MAX_BODY;
/// Refusals sent "busy" that may linger at once, each on a thread of its own; past this a busy
/// refusal closes without lingering, so a flood cannot hold the accept loop or start threads.
const MAX_LINGERING_REFUSALS: usize = 8;

struct Session {
    token: String,
    server: mcp::Server,
    last_used: u64,
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

pub fn serve(listen: Option<&str>, advertise: bool) -> Result<()> {
    if relay::list_tokens()?.is_empty() {
        anyhow::bail!("no tokens yet, so nobody could connect. Add a machine in the Nitely app (Other machines), which shows its token once, then run this again");
    }
    let listen = listen.map(str::to_string).unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
    let listener = TcpListener::bind(&listen).with_context(|| format!("listening on {listen}"))?;
    let local = listener.local_addr()?;
    let port = local.port();
    if let Err(e) = relay::record_serving_addr(local) {
        eprintln!("could not record the address for the app's setup commands ({e:#})");
    }
    let networks = networks();
    let _mdns = if advertise {
        match announce(port, &networks) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("not announced on the network ({e:#}); clients can still connect by address");
                None
            }
        }
    } else {
        None
    };
    println!("Serving the backlog to the local network on port {port} (MCP at {MCP_PATH}).");
    for ip in relay::reachable_addresses(local.ip(), &networks) {
        println!("  {}", url(ip, port, MCP_PATH));
    }
    println!("Only this machine's networks are answered, and only with a token. Ctrl+C stops it.");
    run(listener, Arc::new(networks));
    Ok(())
}

/// The connections being handled, overall and per address.
#[derive(Default)]
struct Slots {
    active: AtomicUsize,
    per_address: Mutex<HashMap<IpAddr, usize>>,
    lingering: AtomicUsize,
}

impl Slots {
    /// Connections `ip` holds now.
    #[cfg(test)]
    fn held(&self, ip: IpAddr) -> usize {
        self.per_address.lock().unwrap_or_else(|p| p.into_inner()).get(&ip).copied().unwrap_or(0)
    }
}

/// Accepts connections until the listener fails, each on its own thread.
fn run(listener: TcpListener, networks: Arc<Vec<(IpAddr, u8)>>) {
    run_with(listener, networks, Arc::new(Slots::default()));
}

fn run_with(listener: TcpListener, networks: Arc<Vec<(IpAddr, u8)>>, slots: Arc<Slots>) {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let tick = Arc::new(AtomicU64::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let peer = stream.peer_addr().ok();
        if !peer.is_some_and(|a| relay::is_local_to(a.ip(), &networks)) {
            // Nothing is said to an address outside the local network; the connection just closes.
            log(peer, "-", "refused: not on the local network", 403);
            continue;
        }
        let ip = peer.map(|a| a.ip()).expect("checked above");
        let admitted = {
            // An address gets an entry only while it holds an admitted connection, so refused
            // connections, from however many addresses, leave nothing behind.
            let mut held = slots.per_address.lock().unwrap_or_else(|p| p.into_inner());
            let mine = held.get(&ip).copied().unwrap_or(0);
            if slots.active.load(Ordering::SeqCst) >= MAX_CONNECTIONS || mine >= MAX_PER_ADDRESS {
                false
            } else {
                held.insert(ip, mine + 1);
                slots.active.fetch_add(1, Ordering::SeqCst);
                true
            }
        };
        if !admitted {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            write_response(&mut stream, 503, "text/plain", &[], b"Busy; try again shortly.");
            if slots.lingering.fetch_add(1, Ordering::SeqCst) < MAX_LINGERING_REFUSALS {
                let slots = slots.clone();
                std::thread::spawn(move || {
                    linger(&mut stream);
                    slots.lingering.fetch_sub(1, Ordering::SeqCst);
                });
            } else {
                slots.lingering.fetch_sub(1, Ordering::SeqCst);
            }
            continue;
        }
        let (sessions, slots, tick) = (sessions.clone(), slots.clone(), tick.clone());
        std::thread::spawn(move || {
            connection(&mut stream, peer, &sessions, tick.fetch_add(1, Ordering::SeqCst) + 1);
            linger(&mut stream);
            drop(stream);
            slots.active.fetch_sub(1, Ordering::SeqCst);
            let mut held = slots.per_address.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(n) = held.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    held.remove(&ip);
                }
            }
        });
    }
}

/// A request's line and headers, and whatever of the body came with them.
struct Head {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    rest: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// Sets the read timeout to what is left before `deadline`; Err once nothing is left, so a client
/// that sends a byte at a time still runs out of time.
fn time_left(stream: &TcpStream, deadline: Instant) -> std::result::Result<(), (u16, &'static str)> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err((408, "Request timed out."));
    }
    stream.set_read_timeout(Some(left)).map_err(|_| (408, "Request timed out."))
}

fn read_head(stream: &mut TcpStream, deadline: Instant) -> std::result::Result<Head, (u16, &'static str)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err((431, "Request headers too large."));
        }
        time_left(stream, deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err((400, "Incomplete request.")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return Err((408, "Request timed out.")),
        }
    };
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| (400, "Request headers are not text."))?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    if method.is_empty() || !target.starts_with('/') {
        return Err((400, "Bad request line."));
    }
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let path = target.split('?').next().unwrap_or("").to_string();
    Ok(Head { method, path, headers, rest: buf[end + 4..].to_vec() })
}

/// The body, by Content-Length only and within MAX_BODY; chunked or oversized bodies are refused
/// without being read.
fn read_body(stream: &mut TcpStream, head: &Head, deadline: Instant) -> std::result::Result<Vec<u8>, (u16, &'static str)> {
    if head.header("transfer-encoding").is_some() {
        return Err((411, "Send the body with a Content-Length."));
    }
    let len: usize = match head.header("content-length") {
        Some(v) => v.trim().parse().map_err(|_| (400, "Bad Content-Length."))?,
        None => return Err((411, "Send the body with a Content-Length.")),
    };
    if len > MAX_BODY {
        return Err((413, "Request too large."));
    }
    let mut body = head.rest.clone();
    body.truncate(len);
    let mut got = body.len();
    body.resize(len, 0);
    while got < len {
        time_left(stream, deadline)?;
        match stream.read(&mut body[got..]) {
            Ok(0) | Err(_) => return Err((408, "Request body timed out.")),
            Ok(n) => got += n,
        }
    }
    Ok(body)
}

fn connection(stream: &mut TcpStream, from: Option<SocketAddr>, sessions: &Sessions, tick: u64) {
    let deadline = Instant::now() + IO_TIMEOUT;
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let head = match read_head(stream, deadline) {
        Ok(h) => h,
        Err((status, message)) => return write_response(stream, status, "text/plain", &[], message.as_bytes()),
    };
    if head.path == "/.well-known/nitely" && head.method == "GET" {
        let info = json!({ "name": "nitely", "version": env!("CARGO_PKG_VERSION"), "mcp": MCP_PATH, "auth": "bearer" });
        return write_json(stream, 200, &[], &info);
    }
    if head.path != MCP_PATH {
        return write_response(stream, 404, "text/plain", &[], b"Not found; the MCP endpoint is /mcp.");
    }
    if head.method != "POST" {
        // No server-sent stream: every answer comes back on its POST.
        return write_response(stream, 405, "text/plain", &[("Allow", "POST")], b"");
    }
    let bearer = head.header("authorization").and_then(|v| v.split_once(' ')).filter(|(s, _)| s.eq_ignore_ascii_case("bearer")).map(|(_, t)| t.trim().to_string());
    let token = match bearer.map(|b| relay::verify(&b)) {
        Some(Ok(Some(t))) => t,
        Some(Err(e)) => {
            log(from, "-", &format!("token store unreadable: {e:#}"), 500);
            return write_response(stream, 500, "text/plain", &[], b"Nitely could not read its tokens.");
        }
        _ => {
            log(from, "-", "refused: no valid token", 401);
            return write_response(stream, 401, "text/plain", &[("WWW-Authenticate", "Bearer")], b"A Nitely token is required: Authorization: Bearer <token>.");
        }
    };
    let body = match read_body(stream, &head, deadline) {
        Ok(b) => b,
        Err((status, message)) => {
            log(from, &token.name, message, status);
            return write_response(stream, status, "text/plain", &[], message.as_bytes());
        }
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return write_json(stream, 400, &[], &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "send one JSON-RPC message per request" } })),
        Err(e) => return write_json(stream, 400, &[], &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } })),
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let what = match msg.pointer("/params/name").and_then(Value::as_str) {
        Some(tool) => format!("{method} {tool}"),
        None => method.clone(),
    };

    let (status, reply, session_id) = {
        let mut sessions = sessions.lock().unwrap_or_else(|p| p.into_inner());
        // A session starts at initialize and is named by the Mcp-Session-Id header after that.
        let id = if method == "initialize" {
            Some(new_session_id())
        } else {
            match head.header("mcp-session-id") {
                Some(id) if sessions.get(id).is_some_and(|s| s.token == token.name) => Some(id.to_string()),
                Some(_) => None,
                // A client that keeps no session still gets its calls answered, as this token.
                None => Some(String::new()),
            }
        };
        match id {
            None => (404, None, None),
            Some(id) => {
                if !sessions.contains_key(&id) {
                    if sessions.len() >= MAX_SESSIONS {
                        if let Some(oldest) = sessions.iter().min_by_key(|(_, s)| s.last_used).map(|(k, _)| k.clone()) {
                            sessions.remove(&oldest);
                        }
                    }
                    sessions.insert(id.clone(), Session { token: token.name.clone(), server: mcp::Server::remote(&token.name, token.project.clone()), last_used: tick });
                }
                let session = sessions.get_mut(&id).expect("the session was just found or made");
                session.last_used = tick;
                let reply = mcp::handle(&mut session.server, &msg);
                if id.is_empty() {
                    sessions.remove(&id);
                }
                (if reply.is_some() { 200 } else { 202 }, reply, (method == "initialize").then_some(id))
            }
        }
    };
    log(from, &token.name, &what, status);
    match (status, reply) {
        (404, _) => write_response(stream, 404, "text/plain", &[], b"Unknown session; initialize again."),
        (_, Some(r)) => match &session_id {
            Some(id) => write_json(stream, 200, &[("Mcp-Session-Id", id)], &r),
            None => write_json(stream, 200, &[], &r),
        },
        _ => write_response(stream, 202, "text/plain", &[], b""),
    }
}

fn write_json(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &Value) {
    write_response(stream, status, "application/json", headers, body.to_string().as_bytes());
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, headers: &[(&str, &str)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes()).and_then(|_| stream.write_all(body)).and_then(|_| stream.flush());
    // Says the answer is complete; the read side stays open for `linger`.
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// Reads and drops what the client still sends (the rest of a refused request) until it closes,
/// for at most LINGER and MAX_DRAIN, so the close is a clean one and the client gets its answer.
fn linger(stream: &mut TcpStream) {
    let deadline = Instant::now() + LINGER;
    let mut chunk = [0u8; 8192];
    let mut drained = 0;
    while drained < MAX_DRAIN && time_left(stream, deadline).is_ok() {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    // Session ids name a session, they do not grant access (the token does), so the clock is a
    // fallback should the OS have no randomness to give.
    if relay::random_bytes(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (nanos >> ((i % 16) * 8)) as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One line per request, without the token.
fn log(from: Option<SocketAddr>, who: &str, what: &str, status: u16) {
    let from = from.map(|a| a.ip().to_string()).unwrap_or_else(|| "?".into());
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let day = secs % 86_400;
    println!("{:02}:{:02}:{:02}Z {from} {who} {what} -> {status}", day / 3600, day % 3600 / 60, day % 60);
}

fn hostname() -> String {
    let raw = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "nitely".into());
    let label: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let label = label.trim_matches('-').to_string();
    if label.is_empty() { "nitely".into() } else { label }
}

/// Announces this server over mDNS until the returned daemon is dropped.
fn announce(port: u16, networks: &[(IpAddr, u8)]) -> Result<mdns_sd::ServiceDaemon> {
    let daemon = mdns_sd::ServiceDaemon::new().context("starting mDNS")?;
    let host = hostname();
    let props = [("path", MCP_PATH), ("version", env!("CARGO_PKG_VERSION")), ("auth", "bearer")];
    let addresses = lan_addresses(networks);
    let info = mdns_sd::ServiceInfo::new(SERVICE_TYPE, &format!("Nitely on {host}"), &format!("{host}.local."), &addresses[..], port, &props[..])
        .context("describing the service")?
        .enable_addr_auto();
    daemon.register(info).context("announcing the service")?;
    Ok(daemon)
}

/// `nitely discover`: Nitely servers announced on the local network, each with the addresses
/// that answered. mDNS answers can carry only some of a host's addresses (a firewall can drop
/// IPv4 multicast while link-local IPv6 gets through), so the announced host name is also looked
/// up through the system resolver, and every address is checked against the server's
/// `/.well-known/nitely` before it is listed.
pub fn discover(wait: Duration) -> Result<Vec<Value>> {
    let daemon = mdns_sd::ServiceDaemon::new().context("starting mDNS")?;
    let events = daemon.browse(SERVICE_TYPE).context("browsing for Nitely")?;
    let deadline = std::time::Instant::now() + wait;
    // Full name -> (host, port, path, addresses).
    let mut seen: Vec<(String, String, u16, String, Vec<IpAddr>)> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else { break };
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
            let path = info.get_property_val_str("path").unwrap_or(MCP_PATH).to_string();
            let ips: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
            match seen.iter_mut().find(|s| s.0 == info.get_fullname()) {
                Some(entry) => entry.4.extend(ips),
                None => seen.push((info.get_fullname().to_string(), info.get_hostname().to_string(), info.get_port(), path, ips)),
            }
        }
    }
    let _ = daemon.shutdown();

    let mut found = Vec::new();
    for (name, host, port, path, mut ips) in seen {
        use std::net::ToSocketAddrs;
        if let Ok(resolved) = (host.trim_end_matches('.'), port).to_socket_addrs() {
            ips.extend(resolved.map(|a| a.ip()));
        }
        let mut candidates = lan_addresses(&ips.into_iter().map(|ip| (ip, 0)).collect::<Vec<_>>());
        candidates.retain(|ip| !ip.is_loopback());
        let (reachable, unreachable): (Vec<IpAddr>, Vec<IpAddr>) = candidates.into_iter().partition(|ip| answers(*ip, port));
        found.push(json!({
            "name": name,
            "host": host,
            "urls": reachable.iter().map(|ip| url(*ip, port, &path)).collect::<Vec<_>>(),
            "unreachable": unreachable.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
        }));
    }
    Ok(found)
}

/// Whether a Nitely server answers its discovery endpoint at `ip`.
fn answers(ip: IpAddr, port: u16) -> bool {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(800)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let Ok(mut res) = agent.get(&url(ip, port, "/.well-known/nitely")).call() else { return false };
    res.status().as_u16() == 200
        && res.body_mut().read_json::<Value>().ok().is_some_and(|v| v.get("name").and_then(Value::as_str) == Some("nitely"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool call over HTTP, as Claude Code makes it.
    fn post(url: &str, token: Option<&str>, session: Option<&str>, body: Value) -> (u16, Option<String>, Value) {
        let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
        let mut req = agent.post(url).header("Content-Type", "application/json").header("Accept", "application/json, text/event-stream");
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        if let Some(s) = session {
            req = req.header("Mcp-Session-Id", s);
        }
        let mut res = req.send(body.to_string()).unwrap();
        let status = res.status().as_u16();
        let sid = res.headers().get("mcp-session-id").map(|v| v.to_str().unwrap().to_string());
        let text = res.body_mut().read_to_string().unwrap_or_default();
        (status, sid, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    #[test]
    fn serves_the_backlog_to_a_token_and_only_the_backlog() {
        let home = std::env::temp_dir().join(format!("nitely-serve-{}-{}", std::process::id(), nitely_core::db::now()));
        std::fs::create_dir_all(&home).unwrap();
        // This test owns NITELY_HOME for its process; no other test in this crate reads it.
        std::env::set_var("NITELY_HOME", &home);
        let db = nitely_core::Db::open_default().unwrap();
        db.create_project("APP", "Acme").unwrap();
        let token = relay::add_token("mac", Some("APP")).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let slots = Arc::new(Slots::default());
        let server_slots = slots.clone();
        std::thread::spawn(move || run_with(listener, Arc::new(vec![]), server_slots));
        let localhost: IpAddr = [127, 0, 0, 1].into();
        // The server frees a slot just after its client sees the connection close, so wait for
        // the count rather than for a fixed time (NIT-105).
        let settle = |want: usize, why: &str| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while slots.held(localhost) != want {
                assert!(Instant::now() < deadline, "{why}: the address holds {} connections, not {want}", slots.held(localhost));
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        // A client that sends half its headers and stalls holds only its own connection.
        let mut stalled = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stalled.write_all(b"POST /mcp HTTP/1.1\r\nContent-Length: 500\r\n").unwrap();
        let url = format!("http://127.0.0.1:{port}{MCP_PATH}");
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "claude-code" } } });

        assert_eq!(post(&url, None, None, init.clone()).0, 401, "no token, no answer");
        assert_eq!(post(&url, Some("nly_wrong"), None, init.clone()).0, 401);

        let (status, sid, r) = post(&url, Some(&token), None, init);
        assert_eq!(status, 200);
        let sid = sid.expect("a session id");
        assert!(r["result"]["instructions"].as_str().unwrap().contains("local network"));
        assert_eq!(post(&url, Some(&token), Some(&sid), json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).0, 202);

        let (_, _, tools) = post(&url, Some(&token), Some(&sid), json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"file_issue") && names.contains(&"next_task"));
        assert!(!names.contains(&"land") && !names.contains(&"start_task"), "{names:?}");

        let call = |id: i64, name: &str, args: Value| json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } });
        let (_, _, filed) = post(&url, Some(&token), Some(&sid), call(3, "file_issue", json!({ "title": "Filed from the Mac", "kind": "bug", "body": "b", "evidence": "e" })));
        let text = filed["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("APP-1"), "the token's project is the default: {text}");
        let issue = db.issue("APP-1").unwrap().unwrap();
        assert_eq!(issue.source.as_deref(), Some("claude"));
        let timeline = db.timeline("APP-1").unwrap();
        assert!(timeline.iter().any(|e| e.actor == "claude@mac"), "the agent is named after its token: {timeline:?}");

        let (_, _, land) = post(&url, Some(&token), Some(&sid), call(4, "land", json!({ "key": "APP-1" })));
        assert_eq!(land["result"]["isError"], true);

        // Another token cannot use this session.
        let other = relay::add_token("cloud", None).unwrap();
        assert_eq!(post(&url, Some(&other), Some(&sid), json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" })).0, 404);

        // Oversized and chunked bodies are refused without being read.
        let raw = |text: &str| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(text.as_bytes()).unwrap();
            let mut answer = String::new();
            let _ = c.read_to_string(&mut answer);
            answer
        };
        let auth = format!("Authorization: Bearer {token}\r\n");
        assert!(raw(&format!("POST /mcp HTTP/1.1\r\n{auth}Content-Length: 99999999\r\n\r\n")).starts_with("HTTP/1.1 413"));
        assert!(raw(&format!("POST /mcp HTTP/1.1\r\n{auth}Transfer-Encoding: chunked\r\n\r\n")).starts_with("HTTP/1.1 411"));

        // One address holds at most MAX_PER_ADDRESS connections; the next is told busy at once.
        drop(stalled);
        settle(0, "every earlier connection is closed");
        let held: Vec<_> = (0..MAX_PER_ADDRESS)
            .map(|_| {
                let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                c.write_all(b"POST /mcp HTTP/1.1\r\n").unwrap();
                c
            })
            .collect();
        settle(MAX_PER_ADDRESS, "the held ones fill the slots");
        assert!(raw("GET /.well-known/nitely HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 503"), "these fill the address's slots");
        drop(held);
        std::env::remove_var("NITELY_HOME");
    }
}
