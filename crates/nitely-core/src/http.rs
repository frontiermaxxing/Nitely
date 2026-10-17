//! The small, strict HTTP/1.1 layer Nitely's servers share (`nitely serve` for agents on other
//! machines, the app's phone server): one thread per connection with a cap overall and per address,
//! read and write timeouts, a bounded header and body, Content-Length only, and the connection
//! closed after each answer. A slow or hostile client ties up its own thread for seconds, never the
//! server.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Larger bodies are refused unread.
pub const MAX_BODY: usize = 1 << 20;
/// The request line and headers together.
pub const MAX_HEAD: usize = 16 << 10;
/// Connections handled at once; past this, new ones get 503 at once.
pub const MAX_CONNECTIONS: usize = 32;
/// How long a connection may take to send its whole request, however it trickles in, and to read
/// the answer.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections one address may hold at once, so one machine cannot take every slot.
pub const MAX_PER_ADDRESS: usize = 4;
/// After answering, how long and how much of what the client still sends is read and dropped
/// before the connection closes. Closing with unread data makes Windows reset the connection,
/// and the client can lose the answer it was about to read (NIT-120).
const LINGER: Duration = Duration::from_secs(2);
const MAX_DRAIN: usize = MAX_BODY;
/// Refusals sent "busy" that may linger at once, each on a thread of its own; past this a busy
/// refusal closes without lingering, so a flood cannot hold the accept loop or start threads.
const MAX_LINGERING_REFUSALS: usize = 8;

/// The connections being handled, overall and per address.
#[derive(Default)]
pub struct Slots {
    active: AtomicUsize,
    per_address: Mutex<HashMap<IpAddr, usize>>,
    lingering: AtomicUsize,
}

impl Slots {
    /// Connections `ip` holds now.
    pub fn held(&self, ip: IpAddr) -> usize {
        self.per_address.lock().unwrap_or_else(|p| p.into_inner()).get(&ip).copied().unwrap_or(0)
    }
}

/// Handles one admitted connection; the layer lingers and closes it afterwards.
pub type Handler = Arc<dyn Fn(&mut TcpStream, SocketAddr) + Send + Sync>;

/// Accepts connections until the listener fails, each on its own thread. Only peers `allow` takes
/// get anything; the others' connections just close, with nothing said.
pub fn accept(listener: TcpListener, allow: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>, slots: Arc<Slots>, handler: Handler) {
    accept_until(listener, allow, slots, handler, Arc::new(AtomicBool::new(false)));
}

/// As `accept`, until `stop` is set; the loop sees it at the next connection, so whoever sets it
/// connects once to wake it (`Stopper::stop`).
pub fn accept_until(listener: TcpListener, allow: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>, slots: Arc<Slots>, handler: Handler, stop: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut stream) = stream else { continue };
        let Some(peer) = stream.peer_addr().ok().filter(|a| allow(a.ip())) else { continue };
        let ip = peer.ip();
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
        let (slots, handler) = (slots.clone(), handler.clone());
        std::thread::spawn(move || {
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            handler(&mut stream, peer);
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

/// A connection a request is read from and answered on: a plain TCP stream, or TLS over one
/// (the phone server over HTTPS). Timeouts are set on the TCP stream underneath.
pub trait Conn: Read + Write {
    fn tcp(&self) -> &TcpStream;
    /// Says the answer is complete; the read side stays open for `linger`.
    fn close_write(&mut self) {
        let _ = self.tcp().shutdown(std::net::Shutdown::Write);
    }
}

impl Conn for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

/// A request's line and headers, and whatever of the body came with them.
pub struct Head {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub rest: Vec<u8>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// Sets the read timeout to what is left before `deadline`; Err once nothing is left, so a client
/// that sends a byte at a time still runs out of time.
pub fn time_left(stream: &TcpStream, deadline: Instant) -> std::result::Result<(), (u16, &'static str)> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err((408, "Request timed out."));
    }
    stream.set_read_timeout(Some(left)).map_err(|_| (408, "Request timed out."))
}

pub fn read_head<C: Conn + ?Sized>(stream: &mut C, deadline: Instant) -> std::result::Result<Head, (u16, &'static str)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err((431, "Request headers too large."));
        }
        time_left(stream.tcp(), deadline)?;
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
/// without being read. No Content-Length on a GET is an empty body.
pub fn read_body<C: Conn + ?Sized>(stream: &mut C, head: &Head, deadline: Instant) -> std::result::Result<Vec<u8>, (u16, &'static str)> {
    if head.header("transfer-encoding").is_some() {
        return Err((411, "Send the body with a Content-Length."));
    }
    let len: usize = match head.header("content-length") {
        Some(v) => v.trim().parse().map_err(|_| (400, "Bad Content-Length."))?,
        None if head.method == "GET" => 0,
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
        time_left(stream.tcp(), deadline)?;
        match stream.read(&mut body[got..]) {
            Ok(0) | Err(_) => return Err((408, "Request body timed out.")),
            Ok(n) => got += n,
        }
    }
    Ok(body)
}

pub fn write_json<C: Conn + ?Sized>(stream: &mut C, status: u16, headers: &[(&str, &str)], body: &Value) {
    write_response(stream, status, "application/json", headers, body.to_string().as_bytes());
}

pub fn write_response<C: Conn + ?Sized>(stream: &mut C, status: u16, content_type: &str, headers: &[(&str, &str)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
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
    stream.close_write();
}

/// Reads and drops what the client still sends (the rest of a refused request) until it closes,
/// for at most LINGER and MAX_DRAIN, so the close is a clean one and the client gets its answer.
pub fn linger(stream: &mut TcpStream) {
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

/// Stops an `accept_until` loop: sets its flag and connects once so the loop sees it.
pub struct Stopper {
    pub addr: SocketAddr,
    pub flag: Arc<AtomicBool>,
}

impl Stopper {
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
    }
}
