//! `nitely serve`: this machine's backlog for agents on others on the local network. It is the
//! MCP server (`mcp.rs`) over Streamable HTTP at `/mcp`, so Claude Code elsewhere adds it with
//! `claude mcp add --transport http` and needs no Nitely install. Every request must come from a
//! private address and carry a token (`nitely tokens add`); the token's name is the agent's
//! identity. The server announces itself over mDNS as `_nitely._tcp`, which `nitely discover` and
//! macOS's `dns-sd -B _nitely._tcp` find.

use crate::mcp;
use anyhow::{anyhow, Context, Result};
use nitely_core::relay::{self, DEFAULT_PORT, MCP_PATH, SERVICE_TYPE};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response};

/// Larger requests are refused; a tool call is a few kilobytes.
const MAX_BODY: usize = 1 << 20;
/// Open sessions kept; the oldest is dropped past this, and its client starts a new one.
const MAX_SESSIONS: usize = 256;

struct Session {
    token: String,
    server: mcp::Server,
    last_used: u64,
}

pub fn serve(listen: Option<&str>, advertise: bool) -> Result<()> {
    if relay::list_tokens()?.is_empty() {
        anyhow::bail!("no tokens yet, so nobody could connect. Make one per machine first: nitely tokens add mac --project AND");
    }
    let listen = listen.map(str::to_string).unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"));
    let http = tiny_http::Server::http(&listen).map_err(|e| anyhow!("listening on {listen}: {e}"))?;
    let port = http.server_addr().to_ip().map(|a| a.port()).unwrap_or(DEFAULT_PORT);
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
    for ip in lan_addresses(&networks) {
        println!("  {}", url(ip, port, MCP_PATH));
    }
    println!("Only private addresses are answered, and only with a token. Ctrl+C stops it.");

    let mut sessions: HashMap<String, Session> = HashMap::new();
    let mut tick = 0u64;
    for request in http.incoming_requests() {
        tick += 1;
        handle(request, &mut sessions, tick, &networks);
    }
    Ok(())
}

fn handle(mut request: Request, sessions: &mut HashMap<String, Session>, tick: u64, networks: &[(IpAddr, u8)]) {
    let from = request.remote_addr().copied();
    if !from.is_some_and(|a| relay::is_local_to(a.ip(), networks)) {
        log(from, "-", "refused: not on the local network", 403);
        let _ = request.respond(Response::from_string("Nitely answers only the local network.").with_status_code(403));
        return;
    }
    let path = request.url().split('?').next().unwrap_or("").to_string();
    if path == "/.well-known/nitely" && *request.method() == Method::Get {
        let info = json!({ "name": "nitely", "version": env!("CARGO_PKG_VERSION"), "mcp": MCP_PATH, "auth": "bearer" });
        let _ = request.respond(json_response(200, &info));
        return;
    }
    if path != MCP_PATH {
        let _ = request.respond(Response::from_string("Not found; the MCP endpoint is /mcp.").with_status_code(404));
        return;
    }
    if *request.method() != Method::Post {
        // No server-sent stream: every answer comes back on its POST.
        let _ = request.respond(Response::from_string("").with_status_code(405).with_header(header("Allow", "POST")));
        return;
    }
    let token = match bearer(&request).map(|b| relay::verify(&b)) {
        Some(Ok(Some(t))) => t,
        Some(Err(e)) => {
            log(from, "-", &format!("token store unreadable: {e:#}"), 500);
            let _ = request.respond(Response::from_string("Nitely could not read its tokens.").with_status_code(500));
            return;
        }
        _ => {
            log(from, "-", "refused: no valid token", 401);
            let _ = request.respond(
                Response::from_string("A Nitely token is required: Authorization: Bearer <token>.")
                    .with_status_code(401)
                    .with_header(header("WWW-Authenticate", "Bearer")),
            );
            return;
        }
    };
    if request.body_length().is_some_and(|n| n > MAX_BODY) {
        let _ = request.respond(Response::from_string("Request too large.").with_status_code(413));
        return;
    }
    let mut body = Vec::new();
    if request.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        let _ = request.respond(Response::from_string("Request unreadable or too large.").with_status_code(413));
        return;
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => {
            let r = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "send one JSON-RPC message per request" } });
            let _ = request.respond(json_response(400, &r));
            return;
        }
        Err(e) => {
            let r = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } });
            let _ = request.respond(json_response(400, &r));
            return;
        }
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let what = match msg.pointer("/params/name").and_then(Value::as_str) {
        Some(tool) => format!("{method} {tool}"),
        None => method.clone(),
    };

    // A session starts at initialize and is named by the Mcp-Session-Id header after that.
    let given = header_value(&request, "Mcp-Session-Id");
    let (id, fresh) = if method == "initialize" {
        (new_session_id(), true)
    } else {
        match given {
            Some(id) if sessions.get(&id).is_some_and(|s| s.token == token.name) => (id, false),
            Some(_) => {
                log(from, &token.name, &format!("{what}: unknown session"), 404);
                let _ = request.respond(Response::from_string("Unknown session; initialize again.").with_status_code(404));
                return;
            }
            // A client that keeps no session still gets its calls answered, as this token.
            None => (String::new(), true),
        }
    };
    if fresh {
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
    let status = if reply.is_some() { 200 } else { 202 };
    log(from, &token.name, &what, status);
    let response = match reply {
        Some(r) => json_response(200, &r),
        None => Response::from_string("").with_status_code(202),
    };
    let response = if method == "initialize" { response.with_header(header("Mcp-Session-Id", &id)) } else { response };
    let _ = request.respond(response);
}

fn bearer(request: &Request) -> Option<String> {
    let value = header_value(request, "Authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("a valid header")
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.to_string()).with_status_code(status).with_header(header("Content-Type", "application/json"))
}

fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom_fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn getrandom_fill(bytes: &mut [u8]) {
    // Session ids name a session, they do not grant access (the token does), so the clock is a
    // fallback should the OS have no randomness to give.
    if nitely_core::relay::random_bytes(bytes).is_err() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (nanos >> ((i % 16) * 8)) as u8;
        }
    }
}

/// One line per request, without the token.
fn log(from: Option<SocketAddr>, who: &str, what: &str, status: u16) {
    let from = from.map(|a| a.ip().to_string()).unwrap_or_else(|| "?".into());
    let at = chrono_free_time();
    println!("{at} {from} {who} {what} -> {status}");
}

fn chrono_free_time() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}Z", day / 3600, day % 3600 / 60, day % 60)
}

/// This machine's networks: each interface's address and prefix length, loopback left out.
fn networks() -> Vec<(IpAddr, u8)> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) => (IpAddr::V4(a.ip), a.prefixlen),
            if_addrs::IfAddr::V6(a) => (IpAddr::V6(a.ip), a.prefixlen),
        })
        .collect()
}

/// The addresses to connect to and announce: IPv4 first, then IPv6 that is not link-local (a
/// link-local one needs its interface named, which a URL cannot carry reliably).
fn lan_addresses(networks: &[(IpAddr, u8)]) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = networks
        .iter()
        .map(|(ip, _)| *ip)
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !v4.is_link_local(),
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
        })
        .collect();
    ips.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    ips.dedup();
    ips
}

fn url(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}{path}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}{path}"),
    }
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
        db.create_project("AND", "Aera").unwrap();
        let token = relay::add_token("mac", Some("AND")).unwrap();

        let http = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = http.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            let mut sessions = HashMap::new();
            for (tick, request) in http.incoming_requests().enumerate() {
                handle(request, &mut sessions, tick as u64, &[]);
            }
        });
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
        assert!(text.contains("AND-1"), "the token's project is the default: {text}");
        let issue = db.issue("AND-1").unwrap().unwrap();
        assert_eq!(issue.source.as_deref(), Some("claude"));
        let timeline = db.timeline("AND-1").unwrap();
        assert!(timeline.iter().any(|e| e.actor == "claude@mac"), "the agent is named after its token: {timeline:?}");

        let (_, _, land) = post(&url, Some(&token), Some(&sid), call(4, "land", json!({ "key": "AND-1" })));
        assert_eq!(land["result"]["isError"], true);

        // Another token cannot use this session.
        let other = relay::add_token("cloud", None).unwrap();
        assert_eq!(post(&url, Some(&other), Some(&sid), json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" })).0, 404);
        std::env::remove_var("NITELY_HOME");
    }
}
