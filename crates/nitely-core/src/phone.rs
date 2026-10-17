//! Deciding from a paired phone (docs/phone.md): the phone server the app runs, pairing a phone,
//! and the device list. The app owns all of it: it starts the server on the addresses the person
//! picks, makes pairing offers, confirms or declines pairing requests, and signs the device list
//! with its approval key. Nothing here is reachable from the CLI or the MCP tools.

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::approval::{hex, sign, unhex, verify};
use crate::db::now;
use crate::http::{self, read_body, read_head, write_json, write_response, Head, Slots, Stopper, IO_TIMEOUT};

/// The phone server's port, beside `nitely serve`'s 7421.
pub const PORT: u16 = 7422;
/// How long a pairing offer stays open.
pub const OFFER_SECONDS: i64 = 600;
/// How far a signed request's time may be from the server's clock.
const SKEW: i64 = 60;
/// How long a request's signature is remembered, so it cannot be sent twice.
const SEEN_FOR: i64 = 2 * SKEW;
/// Pairing requests kept at once; a flood of them cannot grow without bound.
const MAX_REQUESTS: usize = 16;
const DEVICES_ACCOUNT: &str = "phone-devices";

/// What a phone may be served over: an address whose path to the phone is encrypted and
/// authenticated end to end, so nobody in between can replace the page that holds the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    /// This machine only, for trying it out.
    Loopback,
    /// A WireGuard tunnel such as Tailscale's.
    Tunnel,
}

/// Whether `ip` can serve the phone page, and as what, given the addresses the Tailscale client
/// says are this machine's. None for anything else: plain HTTP on an ordinary network could be
/// changed by anyone who can intercept traffic (docs/phone.md). An address in Tailscale's range
/// that Tailscale does not claim is not enough: a carrier's NAT uses 100.64.0.0/10 too.
pub fn path_kind(ip: IpAddr, tailscale: &[IpAddr]) -> Option<PathKind> {
    if ip.is_loopback() {
        Some(PathKind::Loopback)
    } else if tailscale.contains(&ip) && in_tailscale_range(ip) {
        Some(PathKind::Tunnel)
    } else {
        None
    }
}

/// Tailscale's address ranges: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
pub fn in_tailscale_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// This machine's addresses on its tailnet, as the Tailscale client reports them (`tailscale ip`);
/// empty when Tailscale is not installed or not signed in.
pub fn tailscale_ips() -> Vec<IpAddr> {
    match tailscale(&["ip"]) {
        Ok(out) => out.lines().filter_map(|l| l.trim().parse().ok()).filter(|ip| in_tailscale_range(*ip)).collect(),
        Err(_) => vec![],
    }
}

/// Tailscale on this machine, for the Phone screen's "Away from home": whether it is installed and
/// signed in, this machine's tailnet name and addresses, and the phones on the tailnet.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TailscaleState {
    pub installed: bool,
    /// Signed in and connected (`BackendState` "Running").
    pub running: bool,
    /// What Tailscale says when it is not running ("NeedsLogin", "Stopped").
    pub backend: String,
    /// This machine's MagicDNS name, without the trailing dot.
    pub name: Option<String>,
    pub ips: Vec<String>,
    /// The phones and tablets on the tailnet: their name and whether Tailscale sees them online.
    pub phones: Vec<TailnetPhone>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TailnetPhone {
    pub name: String,
    pub online: bool,
}

/// Reads `tailscale status --json`.
pub fn parse_tailscale_status(json: &str) -> Result<TailscaleState> {
    let v: Value = serde_json::from_str(json).context("tailscale status said something unexpected")?;
    let backend = v["BackendState"].as_str().unwrap_or("").to_string();
    let me = &v["Self"];
    let name = me["DNSName"].as_str().map(|n| n.trim_end_matches('.').to_string()).filter(|n| !n.is_empty());
    let ips = me["TailscaleIPs"].as_array().map(|a| a.iter().filter_map(|ip| ip.as_str().map(String::from)).collect()).unwrap_or_default();
    let mut phones: Vec<TailnetPhone> = v["Peer"]
        .as_object()
        .map(|peers| {
            peers
                .values()
                .filter(|p| matches!(p["OS"].as_str().map(str::to_ascii_lowercase).as_deref(), Some("ios" | "android")))
                .map(|p| TailnetPhone { name: p["HostName"].as_str().unwrap_or("phone").to_string(), online: p["Online"].as_bool().unwrap_or(false) })
                .collect()
        })
        .unwrap_or_default();
    phones.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
    Ok(TailscaleState { installed: true, running: backend == "Running", backend, name, ips, phones })
}

/// Tailscale on this machine; not installed when its CLI cannot be found.
pub fn tailscale_state() -> TailscaleState {
    match tailscale(&["status", "--json"]) {
        Ok(out) => parse_tailscale_status(&out).unwrap_or(TailscaleState { installed: true, ..Default::default() }),
        // Installed but stopped or signed out, `tailscale status` still answers (with an exit code
        // other than 0); only a missing CLI says nothing.
        Err(e) if e.to_string().contains("not installed") => TailscaleState::default(),
        Err(_) => TailscaleState { installed: true, backend: "Stopped".into(), ..Default::default() },
    }
}

/// Runs the Tailscale CLI, wherever it is installed, and returns what it printed.
fn tailscale(args: &[&str]) -> Result<String> {
    let candidates = ["tailscale", r"C:\Program Files\Tailscale\tailscale.exe", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
    let mut last = None;
    for program in candidates {
        let Ok(out) = crate::playbook::shell_free_command(program).args(args).stdin(std::process::Stdio::null()).output() else { continue };
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        last = Some(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    match last {
        Some(err) => bail!("tailscale {}: {err}", args.join(" ")),
        None => bail!("the Tailscale CLI is not installed on this PC"),
    }
}

/// The phone server's certificate, and the name on it that the phone's link uses. With it, the
/// page is served over HTTPS, so the path to the phone need not be a tunnel: the certificate,
/// which the phone already trusts, is what stops anyone in between from changing the page.
#[derive(Clone)]
pub struct Tls {
    pub config: Arc<rustls::ServerConfig>,
    pub host: String,
}

/// A certificate and key in PEM files, for `host` (a name on the certificate). The files are only
/// read.
pub fn tls_from_files(cert: &std::path::Path, key: &std::path::Path, host: &str) -> Result<Tls> {
    let cert_pem = std::fs::read(cert).with_context(|| format!("reading {}", cert.display()))?;
    let key_pem = std::fs::read(key).with_context(|| format!("reading {}", key.display()))?;
    tls_from_pem(&cert_pem, &key_pem, host, &cert.display().to_string(), &key.display().to_string())
}

/// A certificate chain and key in PEM, named in errors as `cert_name` and `key_name`.
fn tls_from_pem(cert_pem: &[u8], key_pem: &[u8], host: &str, cert_name: &str, key_name: &str) -> Result<Tls> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        bail!("give the name on the certificate, e.g. pc.example.ts.net");
    }
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("{cert_name} is not a PEM certificate"))?;
    if certs.is_empty() {
        bail!("{cert_name} holds no certificate");
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).with_context(|| format!("{key_name} is not a PEM private key"))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the certificate and key do not go together")?;
    Ok(Tls { config: Arc::new(config), host })
}

/// The name Tailscale can issue this machine a certificate for, from `tailscale status`.
pub fn tailscale_cert_name() -> Result<String> {
    let status: Value = serde_json::from_str(&tailscale(&["status", "--json"])?).context("tailscale status said something unexpected")?;
    status
        .get("CertDomains")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
        .and_then(Value::as_str)
        .map(|d| d.trim_end_matches('.').to_string())
        .context("Tailscale gives this machine no certificate yet: in the Tailscale admin console, turn on MagicDNS and HTTPS Certificates (DNS page), then try again")
}

/// A Tailscale HTTPS certificate for this machine (a real one, which phones trust). `tailscale
/// cert` prints it and its key; Nitely keeps them only in memory and writes no key file. Tailscale
/// keeps and renews its own copy.
pub fn tailscale_tls() -> Result<Tls> {
    let name = tailscale_cert_name()?;
    let pem = tailscale(&["cert", "--cert-file", "-", "--key-file", "-", &name])?;
    tls_from_pem(pem.as_bytes(), pem.as_bytes(), &name, "Tailscale's certificate", "Tailscale's key")
}

/// One connection's TCP stream, which refuses to read past the request's deadline: rustls reads the
/// socket as often as a handshake or record needs, so the deadline is kept here rather than by each
/// HTTP-level read, or a client trickling TLS bytes could hold the connection indefinitely.
struct Sock<'a> {
    tcp: &'a mut TcpStream,
    deadline: Instant,
}

impl std::io::Read for Sock<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the request took too long"));
        }
        self.tcp.set_read_timeout(Some(left))?;
        self.tcp.read(buf)
    }
}

impl std::io::Write for Sock<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

/// TLS over one connection's TCP stream.
struct TlsConn<'a> {
    conn: rustls::ServerConnection,
    sock: Sock<'a>,
}

impl std::io::Read for TlsConn<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        rustls::Stream::new(&mut self.conn, &mut self.sock).read(buf)
    }
}

impl std::io::Write for TlsConn<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        rustls::Stream::new(&mut self.conn, &mut self.sock).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        rustls::Stream::new(&mut self.conn, &mut self.sock).flush()
    }
}

impl http::Conn for TlsConn<'_> {
    fn tcp(&self) -> &TcpStream {
        self.sock.tcp
    }
    fn close_write(&mut self) {
        use std::io::Write;
        self.conn.send_close_notify();
        let _ = self.flush();
        let _ = self.sock.tcp.shutdown(std::net::Shutdown::Write);
    }
}

/// One of this machine's addresses, as the app's Phone screen offers it.
#[derive(Debug, Clone, Serialize)]
pub struct Address {
    pub ip: String,
    pub interface: String,
    pub kind: Option<PathKind>,
}

/// This machine's addresses: loopback, then the tunnel ones, then the rest (which cannot serve).
pub fn addresses() -> Vec<Address> {
    let tailscale = tailscale_ips();
    let mut out: Vec<Address> = vec![Address { ip: "127.0.0.1".into(), interface: "this machine".into(), kind: Some(PathKind::Loopback) }];
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        let ip = i.ip();
        if ip.is_loopback() {
            continue;
        }
        if let IpAddr::V6(v6) = ip {
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                continue;
            }
        }
        out.push(Address { ip: ip.to_string(), interface: i.name.clone(), kind: path_kind(ip, &tailscale) });
    }
    out.sort_by_key(|a| (a.kind.is_none(), a.kind != Some(PathKind::Loopback), a.ip.contains(':')));
    out
}

/// The phone page's address on `ip`.
pub fn url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}/phone/"),
        v4 => format!("http://{v4}:{port}/phone/"),
    }
}

/// A paired phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Ed25519, hex.
    pub public_key: String,
    pub paired_at: i64,
}

/// The device list as the keychain keeps it: signed by the app's approval key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<Device>,
    pub signature: String,
}

fn list_message(devices: &[Device]) -> String {
    let mut m = String::from("nitely-devices-v1\n");
    for d in devices {
        m.push_str(&format!("{}\t{}\t{}\t{}\n", d.id, d.public_key, d.paired_at, d.name.replace(['\t', '\n'], " ")));
    }
    m
}

pub fn sign_list(secret: &[u8; 32], devices: Vec<Device>) -> DeviceList {
    let signature = sign(secret, &list_message(&devices));
    DeviceList { devices, signature }
}

/// The list's devices when its signature verifies with the approval key's public half.
pub fn verified(public: &[u8; 32], list: &DeviceList) -> Option<Vec<Device>> {
    verify(public, &list_message(&list.devices), &list.signature).then(|| list.devices.clone())
}

/// The paired phones, from the keychain, checked against the app's approval key. A list nobody
/// signed, or one signed by another key, is refused: no device in it is trusted.
pub fn load_devices() -> Result<Vec<Device>> {
    trusted_devices(&crate::approval::app_key()?)
}

#[cfg(test)]
thread_local! {
    static TEST_DEVICES: std::cell::RefCell<Option<DeviceList>> = const { std::cell::RefCell::new(None) };
}

/// In tests, the device list `trusted_devices` reads on this thread, instead of the keychain.
#[cfg(test)]
pub fn devices_in_tests(list: Option<DeviceList>) {
    TEST_DEVICES.with(|d| *d.borrow_mut() = list);
}

/// The paired phones in the keychain's list, when it verifies with the approval key `public`.
pub fn trusted_devices(public: &[u8; 32]) -> Result<Vec<Device>> {
    #[cfg(test)]
    let text = TEST_DEVICES.with(|d| d.borrow().as_ref().map(|l| serde_json::to_string(l).expect("a list")));
    #[cfg(not(test))]
    let text = crate::approval::keychain_get(DEVICES_ACCOUNT)?;
    let Some(text) = text else { return Ok(vec![]) };
    let list: DeviceList = serde_json::from_str(&text).context("the phone list in the OS keychain is not one Nitely wrote")?;
    verified(public, &list).context("the phone list in the OS keychain is not signed by this machine's Nitely app; pair your phone again")
}

/// Writes the paired phones to the keychain, signed with the app's approval key. Only the app.
pub fn save_devices(secret: &[u8; 32], devices: Vec<Device>) -> Result<()> {
    crate::approval::keychain_set(DEVICES_ACCOUNT, &serde_json::to_string(&sign_list(secret, devices))?)
}

/// The code both screens show for a pairing request, from its secret and the phone's key, e.g.
/// `4821-0937`. The phone works it out itself, so it cannot be told a matching one.
pub fn pairing_code(secret: &[u8; 32], public_key_hex: &str) -> String {
    let digest = Sha256::new().chain_update(secret).chain_update(public_key_hex.to_ascii_lowercase().as_bytes()).finalize();
    let n = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes")) % 100_000_000;
    format!("{:04}-{:04}", n / 10_000, n % 10_000)
}

/// The message a phone signs to show it holds the key it pairs with.
pub fn pair_message(secret_hex: &str, public_key_hex: &str) -> String {
    format!("nitely-pair-v1\n{secret_hex}\n{}", public_key_hex.to_ascii_lowercase())
}

/// The message a phone signs for each request after pairing. The nonce keeps two identical
/// requests in the same second from carrying the same signature.
pub fn request_message(method: &str, path: &str, time: i64, nonce: &str, body: &[u8]) -> String {
    format!("nitely-phone-v1\n{method}\n{path}\n{time}\n{nonce}\n{}", hex(&Sha256::digest(body)))
}

/// What a phone signs to answer an ask.
pub fn answer_message(ask: i64, answer: &str, time: i64) -> String {
    format!("nitely-answer-v1\n{ask}\n{answer}\n{time}")
}

/// What a phone signs to send a change back to the agents.
pub fn sendback_message(key: &str, reason: &str, time: i64) -> String {
    format!("nitely-sendback-v1\n{}\n{reason}\n{time}", key.to_uppercase())
}

/// A QR code for `text`, as SVG.
pub fn qr_svg(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).context("making the QR code")?;
    Ok(code.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).quiet_zone(true).build())
}

fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_hex<const N: usize>() -> Result<([u8; N], String)> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness from the system: {e}"))?;
    Ok((bytes, hex(&bytes)))
}

struct Offer {
    secret: [u8; 32],
    expires: i64,
}

/// A phone asking to pair, waiting for the person's answer in the app.
#[derive(Debug, Clone, Serialize)]
pub struct PairRequest {
    pub id: String,
    pub name: String,
    pub public_key: String,
    pub code: String,
    pub at: i64,
    /// waiting, paired or declined.
    pub status: String,
}

/// An open pairing offer as the app shows it.
#[derive(Debug, Clone, Serialize)]
pub struct OfferView {
    /// The page address with the secret in its fragment, one per address the server listens on.
    pub urls: Vec<String>,
    /// The QR code of the first url.
    pub qr_svg: String,
    pub expires: i64,
}

/// What the phone server serves its files from: `phone.html` and `assets/...`, with their type.
pub type Assets = Arc<dyn Fn(&str) -> Option<(Vec<u8>, String)> + Send + Sync>;
/// The paired phones, as the server checks them on every request.
pub type Devices = Arc<dyn Fn() -> Result<Vec<Device>> + Send + Sync>;
/// Opens Nitely's database for a request that reads or decides.
pub type OpenDb = Arc<dyn Fn() -> Result<crate::Db> + Send + Sync>;
/// Counts what only the app sees waiting on the person: agents waiting at their own prompt and
/// worktrees stopped in a rebase (`waiting_in_worktrees`, from the board), each named by its
/// issue's key, or its folder when it has none.
pub type InWorktrees = Arc<dyn Fn(&crate::Db) -> Vec<String> + Send + Sync>;

/// The board's worktrees that wait on the person: a rebase stopped there, or the agent in it is
/// waiting for input. The desktop Inbox shows each as a decision. Named by issue key, so one
/// issue the server already counts (a stopped land with an agent waiting in it) counts once.
pub fn waiting_in_worktrees(board: &crate::Board) -> Vec<String> {
    board
        .repos
        .iter()
        .flat_map(|r| r.rows.iter())
        .filter(|row| row.needs_me && (row.state.as_ref().is_some_and(|s| s.rebase_in_progress) || row.session.as_ref().is_some_and(|s| s.state == "waiting")))
        .map(|row| row.issue_key.as_ref().map(|k| k.to_uppercase()).unwrap_or_else(|| format!("worktree:{}", row.worktree)))
        .collect()
}

#[derive(Default)]
struct State {
    offer: Option<Offer>,
    requests: Vec<PairRequest>,
    /// Request signatures seen recently, with when.
    seen: HashMap<String, i64>,
}

/// The phone server's state: shared by its connections and the app.
pub struct Phone {
    state: Mutex<State>,
    devices: Devices,
    assets: Assets,
    db: OpenDb,
    in_worktrees: InWorktrees,
    /// Web push to the paired phones that ask for it (push.rs); off until the app sets it.
    push: std::sync::OnceLock<Arc<crate::push::Push>>,
}

/// How often the phone server looks for new decisions to notify the phones of.
const NOTIFY_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// A running phone server; dropping it does not stop it, `stop` does.
pub struct Running {
    pub addrs: Vec<SocketAddr>,
    /// The certificate's name when it serves HTTPS.
    pub host: Option<String>,
    stoppers: Vec<Stopper>,
    /// Stops the thread that notifies the phones.
    notifier: Option<Arc<AtomicBool>>,
}

impl Running {
    /// The phone page's address on each address it listens on: by the certificate's name over
    /// HTTPS (one link), else by address, the ones a phone can reach first (the pairing QR code
    /// carries the first): the Tailscale address before this machine's loopback, IPv4 before IPv6.
    pub fn pages(&self) -> Vec<String> {
        match &self.host {
            Some(host) => {
                let port = self.addrs.first().map_or(PORT, |a| a.port());
                vec![if port == 443 { format!("https://{host}/phone/") } else { format!("https://{host}:{port}/phone/") }]
            }
            None => {
                let mut addrs = self.addrs.clone();
                addrs.sort_by_key(|a| (a.ip().is_loopback(), a.is_ipv6()));
                addrs.iter().map(|a| url(a.ip(), a.port())).collect()
            }
        }
    }

    pub fn stop(&self) {
        for s in &self.stoppers {
            s.stop();
        }
        if let Some(flag) = &self.notifier {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl Phone {
    pub fn new(devices: Devices, assets: Assets, db: OpenDb, in_worktrees: InWorktrees) -> Arc<Phone> {
        Arc::new(Phone { state: Mutex::new(State::default()), devices, assets, db, in_worktrees, push: std::sync::OnceLock::new() })
    }

    /// Turns on web push: phones may subscribe, and while the server runs it notifies them of new
    /// decisions.
    pub fn set_push(&self, push: Arc<crate::push::Push>) {
        let _ = self.push.set(push);
    }

    pub fn push(&self) -> Option<&Arc<crate::push::Push>> {
        self.push.get()
    }

    /// One look for new decisions: tells the subscribed phones when `notifier` says so. Returns
    /// what failed.
    pub fn notify_tick(&self, notifier: &mut crate::push::Notifier) -> Vec<String> {
        let Some(push) = self.push.get() else { return vec![] };
        // Without the phone list nothing is sent, and nothing forgotten: a phone missing from an
        // unreadable list is not a removed one.
        let paired: Vec<String> = match self.devices() {
            Ok(d) => d.into_iter().map(|d| d.id).collect(),
            Err(e) => return vec![format!("the paired phones could not be read: {e:#}")],
        };
        let Ok(db) = (self.db)() else { return vec![] };
        let Ok((v, _, _)) = decisions(&db) else { return vec![] };
        let Some(message) = notifier.tick(Instant::now(), &waiting_lines(&v)) else { return vec![] };
        let delivery = push.notify(&paired, &message, now());
        if delivery.delivered == 0 && !delivery.failed.is_empty() {
            notifier.undelivered();
        }
        delivery.failed
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Listens on each of `ips` at `port`. Without `tls` each must be loopback or this machine's
    /// Tailscale address; with it, any of this machine's addresses, since the certificate guards
    /// the page.
    pub fn start(self: &Arc<Phone>, ips: &[IpAddr], port: u16, tls: Option<Tls>) -> Result<Running> {
        if ips.is_empty() {
            bail!("pick at least one address to listen on");
        }
        if let Some(ip) = ips.iter().find(|ip| ip.is_unspecified()) {
            bail!("{ip} would listen everywhere; pick the addresses one by one");
        }
        if tls.is_none() {
            let tailscale = if ips.iter().all(|ip| ip.is_loopback()) { vec![] } else { tailscale_ips() };
            if let Some(ip) = ips.iter().find(|ip| path_kind(**ip, &tailscale).is_none()) {
                bail!("{ip} is not this machine's Tailscale address, and plain HTTP on any other network could be changed by anyone who can intercept traffic; pick the Tailscale address, or use HTTPS (docs/phone.md)");
            }
        }
        let networks = Arc::new(if tls.is_some() { crate::relay::networks() } else { vec![] });
        let config = tls.as_ref().map(|t| t.config.clone());
        let mut running = Running { addrs: vec![], host: tls.map(|t| t.host), stoppers: vec![], notifier: None };
        for ip in ips {
            let listener = match TcpListener::bind(SocketAddr::new(*ip, port)) {
                Ok(l) => l,
                Err(e) => {
                    running.stop();
                    return Err(e).with_context(|| format!("listening on {ip}:{port}"));
                }
            };
            let addr = listener.local_addr()?;
            let flag = Arc::new(AtomicBool::new(false));
            let me = self.clone();
            let config = config.clone();
            let handler: http::Handler = Arc::new(move |stream: &mut TcpStream, _peer: SocketAddr| match &config {
                Some(config) => {
                    let Ok(conn) = rustls::ServerConnection::new(config.clone()) else { return };
                    let deadline = Instant::now() + http::IO_TIMEOUT;
                    me.connection(&mut TlsConn { conn, sock: Sock { tcp: stream, deadline } });
                }
                None => me.connection(stream),
            });
            // Loopback answers only this machine; otherwise the tunnel's peers, and over HTTPS also
            // this machine's networks.
            let own = *ip;
            let networks = networks.clone();
            let allow = Arc::new(move |peer: IpAddr| {
                if own.is_loopback() {
                    peer.is_loopback()
                } else {
                    in_tailscale_range(peer) || (!networks.is_empty() && crate::relay::is_local_to(peer, &networks))
                }
            });
            let stop = flag.clone();
            std::thread::spawn(move || http::accept_until(listener, allow, Arc::new(Slots::default()), handler, stop));
            running.addrs.push(addr);
            running.stoppers.push(Stopper { addr, flag });
        }
        if self.push.get().is_some() {
            let flag = Arc::new(AtomicBool::new(false));
            let (me, stop) = (self.clone(), flag.clone());
            std::thread::spawn(move || {
                let mut notifier = crate::push::Notifier::default();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for failed in me.notify_tick(&mut notifier) {
                        eprintln!("nitely: phone notification: {failed}");
                    }
                    let until = Instant::now() + NOTIFY_EVERY;
                    while Instant::now() < until && !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                }
            });
            running.notifier = Some(flag);
        }
        Ok(running)
    }

    /// A new pairing offer, replacing any open one; the secret is only in what this returns.
    /// `pages` are the phone page's addresses (`Running::pages`).
    pub fn offer(&self, pages: &[String]) -> Result<OfferView> {
        let (secret, secret_hex) = random_hex::<32>()?;
        let expires = now() + OFFER_SECONDS;
        self.state().offer = Some(Offer { secret, expires });
        let urls: Vec<String> = pages.iter().map(|p| format!("{p}#pair={secret_hex}")).collect();
        let qr_svg = match urls.first() {
            Some(u) => qr_svg(u)?,
            None => String::new(),
        };
        Ok(OfferView { urls, qr_svg, expires })
    }

    /// Closes the open offer, if any.
    pub fn cancel_offer(&self) {
        self.state().offer = None;
    }

    /// Pairing requests the person has not answered yet.
    pub fn waiting(&self) -> Vec<PairRequest> {
        let cutoff = now() - OFFER_SECONDS;
        self.state().requests.iter().filter(|r| r.status == "waiting" && r.at >= cutoff).cloned().collect()
    }

    /// The device a waiting pairing request would add, without answering it: the app saves it to
    /// the signed list first, then calls `decide`, so a failed save leaves the request waiting.
    pub fn pending_device(&self, id: &str) -> Result<Device> {
        let cutoff = now() - OFFER_SECONDS;
        let state = self.state();
        let r = state.requests.iter().find(|r| r.id == id && r.status == "waiting" && r.at >= cutoff).context("that pairing request is gone; pair the phone again")?;
        Ok(Device { id: r.id.clone(), name: r.name.clone(), public_key: r.public_key.clone(), paired_at: now() })
    }

    /// Marks a request paired after the app saved its device, whatever the clock says by then: the
    /// device is in the list, so the phone must hear it is paired.
    pub fn mark_paired(&self, id: &str) {
        if let Some(r) = self.state().requests.iter_mut().find(|r| r.id == id) {
            r.status = "paired".into();
        }
    }

    /// The person's answer to a pairing request, in the app. Returns the device to add on yes.
    pub fn decide(&self, id: &str, accept: bool) -> Result<Option<Device>> {
        let mut state = self.state();
        let cutoff = now() - OFFER_SECONDS;
        let request = state.requests.iter_mut().find(|r| r.id == id && r.status == "waiting").context("that pairing request is gone; pair the phone again")?;
        if request.at < cutoff {
            request.status = "declined".into();
            bail!("that pairing request is older than {} minutes; pair the phone again", OFFER_SECONDS / 60);
        }
        request.status = if accept { "paired" } else { "declined" }.into();
        Ok(accept.then(|| Device { id: request.id.clone(), name: request.name.clone(), public_key: request.public_key.clone(), paired_at: now() }))
    }

    fn connection<C: http::Conn + ?Sized>(&self, stream: &mut C) {
        let deadline = Instant::now() + IO_TIMEOUT;
        let head = match read_head(stream, deadline) {
            Ok(h) => h,
            Err((status, message)) => return write_response(stream, status, "text/plain", &[], message.as_bytes()),
        };
        let body = match read_body(stream, &head, deadline) {
            Ok(b) => b,
            Err((status, message)) => return write_response(stream, status, "text/plain", &[], message.as_bytes()),
        };
        let (status, reply) = self.route(&head, &body);
        match reply {
            Reply::Json(v) => write_json(stream, status, &[("Cache-Control", "no-store")], &v),
            Reply::File(bytes, kind) => {
                let csp = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
                write_response(stream, status, &kind, &[("Content-Security-Policy", csp), ("X-Content-Type-Options", "nosniff"), ("Referrer-Policy", "no-referrer")], &bytes)
            }
        }
    }

    fn route(&self, head: &Head, body: &[u8]) -> (u16, Reply) {
        let path = head.path.as_str();
        match (head.method.as_str(), path) {
            ("GET", "/phone") | ("GET", "/phone/") | ("GET", "/phone/index.html") => self.file("phone.html"),
            ("GET", p) if p.starts_with("/assets/") && !p.contains("..") => self.file(&p[1..]),
            // The service worker that shows notifications, and what installs the page on the home
            // screen (iPhones show web push only to a page added there).
            ("GET", "/phone/sw.js") | ("GET", "/phone/manifest.webmanifest") | ("GET", "/phone/icon-192.png") | ("GET", "/phone/icon-512.png") => self.file(&path[1..]),
            ("GET", "/phone/api/push") | ("POST", "/phone/api/push") => self.push_route(head, body),
            ("POST", "/phone/api/pair") => self.pair(body),
            ("GET", "/phone/api/pair/status") => self.pair_status(head, body),
            ("GET", "/phone/api/hello") => match self.authorize(head, body) {
                Ok(device) => (200, Reply::Json(json!({ "name": device.name, "id": device.id }))),
                Err((status, why)) => (status, error(why)),
            },
            ("GET", "/phone/api/decisions") | ("POST", "/phone/api/approve") | ("POST", "/phone/api/answer") | ("POST", "/phone/api/sendback") => {
                let device = match self.authorize(head, body) {
                    Ok(d) => d,
                    Err((status, why)) => return (status, error(why)),
                };
                let mut db = match (self.db)() {
                    Ok(db) => db,
                    Err(_) => return (503, error("The PC could not open Nitely's database; try again.")),
                };
                let result = match path {
                    "/phone/api/decisions" => decisions(&db).map(|(mut v, at_pc, on_phone)| {
                        let mut waiting: std::collections::HashSet<String> = at_pc;
                        waiting.extend((self.in_worktrees)(&db));
                        waiting.retain(|k| !on_phone.contains(k));
                        v["at_pc"] = json!(waiting.len());
                        v
                    }),
                    "/phone/api/approve" => approve(&mut db, &device, body),
                    "/phone/api/answer" => answer(&db, &device, body),
                    _ => send_back(&mut db, &device, body),
                };
                match result {
                    Ok(v) => (200, Reply::Json(v)),
                    Err(Refused(status, why)) => (status, Reply::Json(json!({ "error": why }))),
                }
            }
            _ => (404, error("Not found.")),
        }
    }

    /// The phone's notifications: GET says whether it is subscribed and the key to subscribe under;
    /// POST saves its subscription (`PushSubscription.toJSON()`), or forgets it with no endpoint.
    fn push_route(&self, head: &Head, body: &[u8]) -> (u16, Reply) {
        let device = match self.authorize(head, body) {
            Ok(d) => d,
            Err((status, why)) => return (status, error(why)),
        };
        let Some(push) = self.push.get() else { return (404, error("This Nitely app does not send notifications.")) };
        if head.method == "POST" {
            #[derive(Deserialize)]
            struct Keys {
                p256dh: String,
                auth: String,
            }
            #[derive(Deserialize)]
            struct Subscribe {
                endpoint: Option<String>,
                keys: Option<Keys>,
            }
            let Ok(req) = serde_json::from_slice::<Subscribe>(body) else { return (400, error("Send the push subscription.")) };
            let saved = match (req.endpoint, req.keys) {
                (Some(endpoint), Some(keys)) => push.subscribe(crate::push::Subscription { device: device.id.clone(), endpoint, p256dh: keys.p256dh, auth: keys.auth }),
                (None, _) => push.unsubscribe(&device.id),
                _ => return (400, error("Send the push subscription.")),
            };
            if let Err(e) = saved {
                return (400, Reply::Json(json!({ "error": format!("{e:#}") })));
            }
        }
        (200, Reply::Json(json!({ "subscribed": push.subscribed(&device.id), "key": push.public_key() })))
    }

    fn file(&self, path: &str) -> (u16, Reply) {
        match (self.assets)(path) {
            Some((bytes, kind)) => (200, Reply::File(bytes, kind)),
            None => (404, error("Not found.")),
        }
    }

    fn pair(&self, body: &[u8]) -> (u16, Reply) {
        #[derive(Deserialize)]
        struct Pair {
            secret: String,
            name: String,
            public_key: String,
            signature: String,
        }
        let Ok(req) = serde_json::from_slice::<Pair>(body) else { return (400, error("Send secret, name, public_key and signature.")) };
        let name = req.name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return (400, error("Give the phone a name of up to 40 characters."));
        }
        let (Some(secret), Some(public)) = (unhex::<32>(&req.secret), unhex::<32>(&req.public_key)) else { return (400, error("That is not a pairing link Nitely made.")) };
        let mut state = self.state();
        // The offer closes on the first attempt that names its secret, whatever happens next.
        let Some(offer) = state.offer.take() else { return (410, error("No pairing is open. Press Pair a phone in the Nitely app and scan the new code.")) };
        if !equal(&offer.secret, &secret) {
            state.offer = Some(offer);
            return (403, error("That pairing code is not the open one. Scan the code the Nitely app shows now."));
        }
        if offer.expires < now() {
            return (410, error("That pairing code has expired. Press Pair a phone in the Nitely app again."));
        }
        if !verify(&public, &pair_message(&req.secret.to_ascii_lowercase(), &req.public_key), &req.signature) {
            return (400, error("The phone's signature does not match its key."));
        }
        let Ok((_, id)) = random_hex::<8>() else { return (503, error("No randomness on the PC; try again.")) };
        let code = pairing_code(&secret, &req.public_key);
        state.requests.retain(|r| r.status == "waiting");
        if state.requests.len() >= MAX_REQUESTS {
            state.requests.remove(0);
        }
        state.requests.push(PairRequest { id: id.clone(), name: name.to_string(), public_key: req.public_key.to_ascii_lowercase(), code: code.clone(), at: now(), status: "waiting".into() });
        (200, Reply::Json(json!({ "id": id, "code": code })))
    }

    /// Whether the person has paired the phone yet; signed with the key it asked to pair with.
    fn pair_status(&self, head: &Head, body: &[u8]) -> (u16, Reply) {
        let Some(id) = head.header("x-nitely-device").map(str::to_string) else { return (401, error("Sign the request.")) };
        let request = self.state().requests.iter().rev().find(|r| r.id == id).cloned();
        let paired = self.devices().ok().and_then(|d| d.into_iter().find(|d| d.id == id));
        let key = match (&paired, &request) {
            (Some(d), _) => d.public_key.clone(),
            (None, Some(r)) => r.public_key.clone(),
            (None, None) => return (404, error("No pairing request from this phone. Scan the code in the Nitely app again.")),
        };
        if let Err((status, why)) = self.check_signature(head, body, &key) {
            return (status, error(why));
        }
        let expired = |r: &PairRequest| r.at < now() - OFFER_SECONDS;
        let status = match (paired, request) {
            (Some(_), _) => "paired".to_string(),
            (None, Some(r)) if r.status == "waiting" && expired(&r) => "declined".to_string(),
            (None, Some(r)) if r.status == "paired" => "waiting".to_string(),
            (None, Some(r)) => r.status,
            (None, None) => "declined".to_string(),
        };
        (200, Reply::Json(json!({ "status": status })))
    }

    fn devices(&self) -> Result<Vec<Device>> {
        (self.devices)()
    }

    /// The paired device a signed request comes from.
    pub fn authorize(&self, head: &Head, body: &[u8]) -> std::result::Result<Device, (u16, &'static str)> {
        let id = head.header("x-nitely-device").ok_or((401, "Sign the request."))?;
        let devices = self.devices().map_err(|_| (503, "The Nitely app could not read its phone list; open the Phone screen there."))?;
        let device = devices.into_iter().find(|d| d.id == id).ok_or((401, "This phone is not paired with this Nitely app (or was removed). Pair it again."))?;
        self.check_signature(head, body, &device.public_key)?;
        Ok(device)
    }

    fn check_signature(&self, head: &Head, body: &[u8], public_key_hex: &str) -> std::result::Result<(), (u16, &'static str)> {
        let time: i64 = head.header("x-nitely-time").and_then(|t| t.parse().ok()).ok_or((401, "Sign the request."))?;
        let signature = head.header("x-nitely-signature").ok_or((401, "Sign the request."))?;
        let nonce = head.header("x-nitely-nonce").filter(|n| (8..=64).contains(&n.len()) && n.chars().all(|c| c.is_ascii_hexdigit())).ok_or((401, "Sign the request."))?;
        let t = now();
        if (time - t).abs() > SKEW {
            return Err((401, "The phone's clock is more than a minute off the PC's; set both to the network time."));
        }
        let public = unhex::<32>(public_key_hex).ok_or((401, "Bad key."))?;
        let ok = match (unhex::<64>(signature), VerifyingKey::from_bytes(&public)) {
            (Some(sig), Ok(key)) => key.verify(request_message(&head.method, &head.path, time, nonce, body).as_bytes(), &Signature::from_bytes(&sig)).is_ok(),
            _ => false,
        };
        if !ok {
            return Err((401, "The request's signature does not verify."));
        }
        let mut state = self.state();
        state.seen.retain(|_, at| *at > t - SEEN_FOR);
        if state.seen.insert(signature.to_ascii_lowercase(), t).is_some() {
            return Err((409, "That request was already made."));
        }
        Ok(())
    }
}

/// A decision the server refused, with the status and what to tell the person.
struct Refused(u16, String);

impl From<anyhow::Error> for Refused {
    fn from(e: anyhow::Error) -> Refused {
        Refused(500, format!("{e:#}"))
    }
}

fn refuse(status: u16, why: impl Into<String>) -> Refused {
    Refused(status, why.into())
}

/// A decision's body: what was decided, the phone's time and its signature over the decision.
#[derive(Deserialize)]
struct Signed<T> {
    #[serde(flatten)]
    what: T,
    time: i64,
    signature: String,
}

fn check_decision(device: &Device, message: &str, time: i64, signature: &str) -> std::result::Result<(), Refused> {
    if (time - now()).abs() > SKEW {
        return Err(refuse(401, "The phone's clock is more than a minute off the PC's; set both to the network time."));
    }
    let public = unhex::<32>(&device.public_key).ok_or_else(|| refuse(401, "Bad key."))?;
    if !verify(&public, message, signature) {
        return Err(refuse(400, "The decision's signature does not verify."));
    }
    Ok(())
}

/// An issue as the phone shows it.
/// Each waiting decision as (id, one line) for a notification.
fn waiting_lines(v: &Value) -> Vec<(String, String)> {
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    v["decisions"]
        .as_array()
        .map(|ds| {
            ds.iter()
                .map(|d| {
                    let key = d["issue"]["key"].as_str();
                    let line = match d["kind"].as_str() {
                        Some("ask") => format!("{} asks: {}", key.unwrap_or("An agent"), text(&d["ask"]["question"])),
                        _ => format!("{} waits for your approval: {}", key.unwrap_or("A change"), text(&d["issue"]["title"])),
                    };
                    (text(&d["id"]), line)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn issue_json(i: &crate::IssueRecord) -> Value {
    json!({ "key": i.key, "title": i.title, "kind": i.kind, "priority": i.priority, "state": i.state, "track": i.track })
}

/// What waits on the person that the phone can decide: open asks, and changes in Needs You whose
/// latest review waits for a person's approval. What only the PC can settle is counted in
/// `at_pc`: an issue in Needs You for another reason (a playbook change, no review), the person's
/// own open work, and a land that stopped; the server adds the worktrees that wait on the person
/// (`waiting_in_worktrees`).
/// Also returns the issues counted as waiting at the PC, and those the phone decides, by key.
fn decisions(db: &crate::Db) -> std::result::Result<(Value, std::collections::HashSet<String>, std::collections::HashSet<String>), Refused> {
    let mut out = Vec::new();
    let mut on_phone = std::collections::HashSet::new();
    let asks = db.open_asks()?;
    for a in &asks {
        let issue = a.issue_key.as_deref().map(|k| db.issue(k)).transpose()?.flatten();
        if let Some(i) = &issue {
            on_phone.insert(i.key.to_uppercase());
        }
        out.push(json!({
            "kind": "ask",
            "id": format!("ask-{}", a.id),
            "since": a.asked_at,
            "issue": issue.as_ref().map(issue_json),
            "ask": { "id": a.id, "question": a.question, "options": a.options, "asked_by": a.asked_by },
        }));
    }
    let mut at_pc = std::collections::HashSet::new();
    // An issue's approval shows beside its open asks, never behind them: an agent asking "approve
    // this?" must not stand in for the approval itself (NIT-180).
    for i in db.list_issues(None, false)? {
        if i.state != "Needs You" {
            let own_work = i.owner == "human" && !matches!(i.state_type.as_str(), "completed" | "canceled") && i.state != "Backlog";
            let stopped_land = i.track.station == "landing" && i.track.stuck.is_some();
            if own_work || stopped_land {
                at_pc.insert(i.key.to_uppercase());
            }
            continue;
        }
        let review = db.reviews(&i.key)?.into_iter().next();
        let approvable = i.origin == "nitely"
            && db.proposal_for_issue(&i.key)?.is_none()
            && review.as_ref().is_some_and(|r| r.approver == "human")
            && waited_since(db, &i.key, review.as_ref().map_or(0, |r| r.ts))?;
        let Some(r) = review.filter(|_| approvable) else {
            at_pc.insert(i.key.to_uppercase());
            continue;
        };
        on_phone.insert(i.key.to_uppercase());
        out.push(json!({
            "kind": "approve",
            "id": format!("approve-{}", i.key),
            "since": r.ts,
            "issue": issue_json(&i),
            "patch_id": r.patch_id,
            "reasons": r.reasons,
            "review": { "reviewer": r.reviewer, "verdict": r.verdict, "note": r.note, "findings": r.findings },
        }));
    }
    Ok((json!({ "decisions": out }), at_pc, on_phone))
}

/// Whether the issue has stayed in Needs You since its latest submit (or, with none recorded, since
/// `since`, its latest review): only then is that review's change what it waits on. One sent back
/// and in Needs You again for a question has no change to approve until it is submitted again. By
/// the timeline's order, not its whole-second times, so a send-back and a resubmit in one second
/// still read in the order they happened.
fn waited_since(db: &crate::Db, key: &str, since: i64) -> Result<bool> {
    let timeline = db.timeline(key)?;
    let left = |t: &crate::issues::TimelineEntry| {
        t.kind == "issue_moved" && t.summary.as_deref().is_some_and(|s| s.starts_with("Needs You → ") && !s.starts_with("Needs You → Needs You"))
    };
    Ok(match timeline.iter().rposition(|t| t.kind == "submit") {
        Some(submit) => !timeline[submit..].iter().any(left),
        None => !timeline.iter().any(|t| t.ts >= since && left(t)),
    })
}

/// Approves a change for the person, signed by the phone over the same message the app signs
/// (docs/approvals.md), with the phone's own time.
fn approve(db: &mut crate::Db, device: &Device, body: &[u8]) -> std::result::Result<Value, Refused> {
    #[derive(Deserialize)]
    struct Approve {
        key: String,
        patch: String,
    }
    let req: Signed<Approve> = serde_json::from_slice(body).map_err(|_| refuse(400, "Send key, patch, time and signature."))?;
    check_decision(device, &crate::approval::message(&req.what.key, &req.what.patch, req.time), req.time, &req.signature)?;
    let issue = db.issue(&req.what.key)?.ok_or_else(|| refuse(404, "That issue is gone."))?;
    // Signed for the key the phone saw; under a project's new key it would not verify at land.
    if !issue.key.eq_ignore_ascii_case(&req.what.key) {
        return Err(refuse(409, format!("{} is {} now; look at it again.", req.what.key, issue.key)));
    }
    if issue.state != "Needs You" || issue.origin != "nitely" || db.proposal_for_issue(&issue.key)?.is_some() {
        return Err(refuse(409, format!("{} no longer waits for your approval here.", issue.key)));
    }
    let review = db.reviews(&issue.key)?.into_iter().next().ok_or_else(|| refuse(409, "It has no review to approve."))?;
    if review.patch_id != req.what.patch {
        return Err(refuse(409, format!("{} changed since you looked; it was reviewed again. Look at it once more.", issue.key)));
    }
    db.move_issue(&issue.key, "Approved", None, None, &crate::Actor::Human)?;
    let at = db.human_approval_at(&issue.key)?.ok_or_else(|| refuse(500, "The approval was not recorded."))?;
    db.record_phone_approval(&issue.key, &req.what.patch, at, &device.id, req.time, &req.signature)?;
    db.add_note(&issue.key, &format!("Approved from the phone “{}”.", device.name), "note", &crate::Actor::Human)?;
    Ok(json!({ "ok": true, "state": "Approved" }))
}

fn answer(db: &crate::Db, device: &Device, body: &[u8]) -> std::result::Result<Value, Refused> {
    #[derive(Deserialize)]
    struct Answer {
        ask: i64,
        answer: String,
    }
    let req: Signed<Answer> = serde_json::from_slice(body).map_err(|_| refuse(400, "Send ask, answer, time and signature."))?;
    let text = req.what.answer.trim();
    if text.is_empty() || text.chars().count() > 4000 {
        return Err(refuse(400, "Give an answer of up to 4000 characters."));
    }
    check_decision(device, &answer_message(req.what.ask, &req.what.answer, req.time), req.time, &req.signature)?;
    let ask = db.ask(req.what.ask)?.ok_or_else(|| refuse(404, "That question is gone."))?;
    if ask.answer.is_some() {
        return Err(refuse(409, "That question was already answered."));
    }
    db.answer_ask(ask.id, text, &crate::Actor::Human)?;
    if let Some(k) = &ask.issue_key {
        db.add_note(k, &format!("Answered from the phone “{}”.", device.name), "note", &crate::Actor::Human)?;
    }
    Ok(json!({ "ok": true }))
}

fn send_back(db: &mut crate::Db, device: &Device, body: &[u8]) -> std::result::Result<Value, Refused> {
    #[derive(Deserialize)]
    struct SendBack {
        key: String,
        reason: String,
    }
    let req: Signed<SendBack> = serde_json::from_slice(body).map_err(|_| refuse(400, "Send key, reason, time and signature."))?;
    let reason = req.what.reason.trim();
    if reason.is_empty() || reason.chars().count() > 4000 {
        return Err(refuse(400, "Say why, in up to 4000 characters."));
    }
    check_decision(device, &sendback_message(&req.what.key, &req.what.reason, req.time), req.time, &req.signature)?;
    let issue = db.issue(&req.what.key)?.ok_or_else(|| refuse(404, "That issue is gone."))?;
    if issue.state != "Needs You" || issue.origin != "nitely" {
        return Err(refuse(409, format!("{} no longer waits for you here.", issue.key)));
    }
    db.move_issue(&issue.key, "Todo", None, None, &crate::Actor::Human)?;
    db.add_note(&issue.key, &format!("Sent back from the phone “{}”: {reason}", device.name), "note", &crate::Actor::Human)?;
    Ok(json!({ "ok": true, "state": "Todo" }))
}

enum Reply {
    Json(Value),
    File(Vec<u8>, String),
}

fn error(message: &str) -> Reply {
    Reply::Json(json!({ "error": message }))
}

/// The type of a file the phone server serves, by its extension.
pub fn content_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{new_secret, public_of};

    #[test]
    fn only_loopback_and_tunnel_addresses_serve() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let ts = [ip("100.101.102.103"), ip("fd7a:115c:a1e0::1")];
        assert_eq!(path_kind(ip("127.0.0.1"), &[]), Some(PathKind::Loopback));
        assert_eq!(path_kind(ip("100.101.102.103"), &ts), Some(PathKind::Tunnel));
        assert_eq!(path_kind(ip("100.101.102.104"), &ts), None, "in the range, but not Tailscale's: a carrier's NAT");
        assert_eq!(path_kind(ip("100.128.0.1"), &[ip("100.128.0.1")]), None, "outside 100.64.0.0/10");
        assert_eq!(path_kind(ip("fd7a:115c:a1e0::1"), &ts), Some(PathKind::Tunnel));
        assert_eq!(path_kind(ip("192.168.1.20"), &ts), None);
        let phone = Phone::new(Arc::new(|| Ok(vec![])), Arc::new(|_: &str| None), Arc::new(|| anyhow::bail!("no database in this test")), Arc::new(|_: &crate::Db| vec![]));
        let err = phone.start(&[ip("192.168.1.20")], 0, None).err().unwrap().to_string();
        assert!(err.contains("not this machine's Tailscale address"), "{err}");
    }

    #[test]
    fn the_device_list_counts_only_when_the_app_signed_it() {
        let app = new_secret().unwrap();
        let d = Device { id: "a1".into(), name: "Pixel".into(), public_key: hex(&public_of(&new_secret().unwrap())), paired_at: 5 };
        let list = sign_list(&app, vec![d.clone()]);
        assert_eq!(verified(&public_of(&app), &list), Some(vec![d.clone()]));
        let mut added = list.clone();
        added.devices.push(Device { id: "evil".into(), ..d.clone() });
        assert_eq!(verified(&public_of(&app), &added), None, "a device added by anything but the app");
        assert_eq!(verified(&public_of(&new_secret().unwrap()), &list), None, "another app's key");
    }

    /// A phone: its key, and requests signed with it.
    struct FakePhone {
        key: ed25519_dalek::SigningKey,
        id: String,
    }

    impl FakePhone {
        fn public(&self) -> String {
            hex(&self.key.verifying_key().to_bytes())
        }
        fn sign(&self, m: &str) -> String {
            use ed25519_dalek::Signer;
            hex(&self.key.sign(m.as_bytes()).to_bytes())
        }
        fn get(&self, base: &str, path: &str, time: i64) -> (u16, Value) {
            self.get_with(base, path, time, "0123456789abcdef")
        }
        fn get_with(&self, base: &str, path: &str, time: i64, nonce: &str) -> (u16, Value) {
            let sig = self.sign(&request_message("GET", path, time, nonce, b""));
            let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
            let mut res = agent
                .get(&format!("{base}{path}"))
                .header("X-Nitely-Device", &self.id)
                .header("X-Nitely-Time", &time.to_string())
                .header("X-Nitely-Nonce", nonce)
                .header("X-Nitely-Signature", &sig)
                .call()
                .unwrap();
            (res.status().as_u16(), res.body_mut().read_json().unwrap_or(Value::Null))
        }
        fn post(&self, base: &str, path: &str, body: Value) -> (u16, Value) {
            let body = body.to_string();
            let time = now();
            let nonce = hex(&new_secret().unwrap()[..8]);
            let sig = self.sign(&request_message("POST", path, time, &nonce, body.as_bytes()));
            let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
            let mut res = agent
                .post(&format!("{base}{path}"))
                .header("Content-Type", "application/json")
                .header("X-Nitely-Device", &self.id)
                .header("X-Nitely-Time", &time.to_string())
                .header("X-Nitely-Nonce", &nonce)
                .header("X-Nitely-Signature", &sig)
                .send(body)
                .unwrap();
            (res.status().as_u16(), res.body_mut().read_json().unwrap_or(Value::Null))
        }
    }

    fn post(url: &str, body: Value) -> (u16, Value) {
        let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
        let mut res = agent.post(url).header("Content-Type", "application/json").send(body.to_string()).unwrap();
        (res.status().as_u16(), res.body_mut().read_json().unwrap_or(Value::Null))
    }

    #[test]
    fn pairs_a_phone_the_person_confirms_and_then_answers_only_its_signed_requests() {
        let app = new_secret().unwrap();
        let store: Arc<Mutex<Option<DeviceList>>> = Arc::new(Mutex::new(None));
        let read = store.clone();
        let public = public_of(&app);
        let devices: Devices = Arc::new(move || Ok(read.lock().unwrap().as_ref().and_then(|l| verified(&public, l)).unwrap_or_default()));
        let assets: Assets = Arc::new(|p: &str| (p == "phone.html").then(|| (b"<html>phone</html>".to_vec(), content_type(p))));
        let phone = Phone::new(devices, assets, Arc::new(|| anyhow::bail!("no database in this test")), Arc::new(|_: &crate::Db| vec![]));
        let running = phone.start(&["127.0.0.1".parse().unwrap()], 0, None).unwrap();
        let base = format!("http://{}", running.addrs[0]);

        let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
        let mut page = agent.get(&format!("{base}/phone/")).call().unwrap();
        assert_eq!(page.status().as_u16(), 200);
        assert!(page.headers().get("content-security-policy").is_some());
        assert_eq!(page.body_mut().read_to_string().unwrap(), "<html>phone</html>");

        let offer = phone.offer(&running.pages()).unwrap();
        let secret = offer.urls[0].split("#pair=").nth(1).unwrap().to_string();
        assert!(offer.qr_svg.starts_with("<?xml") || offer.qr_svg.contains("<svg"));
        let mut p = FakePhone { key: ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap()), id: String::new() };
        let pair = |p: &FakePhone, secret: &str| {
            post(
                &format!("{base}/phone/api/pair"),
                json!({ "secret": secret, "name": "Pixel", "public_key": p.public(), "signature": p.sign(&pair_message(secret, &p.public())) }),
            )
        };
        // A wrong secret leaves the offer open; the right one closes it, once.
        assert_eq!(pair(&p, &"00".repeat(32)).0, 403);
        let (status, r) = pair(&p, &secret);
        assert_eq!(status, 200, "{r}");
        p.id = r["id"].as_str().unwrap().to_string();
        assert_eq!(r["code"], pairing_code(&unhex::<32>(&secret).unwrap(), &p.public()));
        assert_eq!(pair(&p, &secret).0, 410, "the secret pairs one attempt");

        let t = now();
        assert_eq!(p.get(&base, "/phone/api/pair/status", t).1["status"], "waiting");
        assert_eq!(p.get(&base, "/phone/api/hello", t + 1).0, 401, "not paired until the person says so");
        assert_eq!(phone.waiting().len(), 1);
        let device = phone.decide(&p.id, true).unwrap().unwrap();
        *store.lock().unwrap() = Some(sign_list(&app, vec![device]));
        assert_eq!(p.get(&base, "/phone/api/pair/status", t + 2).1["status"], "paired");

        let (status, hello) = p.get(&base, "/phone/api/hello", t + 3);
        assert_eq!((status, hello["name"].as_str()), (200, Some("Pixel")));
        assert_eq!(p.get(&base, "/phone/api/hello", t + 3).0, 409, "the same signed request twice");
        assert_eq!(p.get_with(&base, "/phone/api/hello", t + 3, "fedcba9876543210").0, 200, "the same second, another nonce");
        assert_eq!(p.get(&base, "/phone/api/hello", t - 300).0, 401, "a request from five minutes ago");
        let stranger = FakePhone { key: ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap()), id: p.id.clone() };
        assert_eq!(stranger.get(&base, "/phone/api/hello", t + 4).0, 401, "another key under the paired id");

        // Revoked: the list no longer holds it.
        *store.lock().unwrap() = Some(sign_list(&app, vec![]));
        assert_eq!(p.get(&base, "/phone/api/hello", t + 5).0, 401);
        running.stop();
    }

    #[test]
    fn a_subscribed_phone_is_notified_of_a_new_decision_end_to_end() {
        use crate::issues::{Actor, NewIssue};
        use base64::Engine;
        use ring::agreement;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let app = new_secret().unwrap();
        let public = public_of(&app);
        let p = FakePhone { key: ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap()), id: "d1".into() };
        let list = sign_list(&app, vec![Device { id: p.id.clone(), name: "Pixel".into(), public_key: p.public(), paired_at: now() }]);
        let devices: Devices = Arc::new(move || Ok(verified(&public, &list).unwrap_or_default()));
        let path = crate::paths::unique_temp("nitely-phone-push", ".db");
        let db_path = path.clone();
        let phone = Phone::new(devices, Arc::new(|_: &str| None), Arc::new(move || crate::Db::open(&db_path)), Arc::new(|_: &crate::Db| vec![]));
        let store = crate::paths::unique_temp("nitely-phone-push", ".json");
        let sent: Arc<Mutex<Vec<(String, Vec<u8>)>>> = Arc::new(Mutex::new(vec![]));
        let log = sent.clone();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &ring::rand::SystemRandom::new()).unwrap();
        let push = crate::push::Push::new(
            pkcs8.as_ref(),
            store.clone(),
            Arc::new(move |endpoint: &str, _: &[(&'static str, String)], body: &[u8]| {
                log.lock().unwrap().push((endpoint.to_string(), body.to_vec()));
                Ok(201)
            }),
        )
        .unwrap();
        phone.set_push(Arc::new(push));
        let running = phone.start(&["127.0.0.1".parse().unwrap()], 0, None).unwrap();
        let base = format!("http://{}", running.addrs[0]);

        let (status, state) = p.get(&base, "/phone/api/push", now());
        assert_eq!((status, &state["subscribed"]), (200, &json!(false)), "{state}");
        assert_eq!(b64.decode(state["key"].as_str().unwrap()).unwrap().len(), 65);
        let mine = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &ring::rand::SystemRandom::new()).unwrap();
        let ua_public = mine.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; 16];
        let keys = json!({ "p256dh": b64.encode(&ua_public), "auth": b64.encode(auth) });
        let (status, r) = p.post(&base, "/phone/api/push", json!({ "endpoint": "https://example.com/push/1", "keys": keys }));
        assert_eq!(status, 400, "only a push service: {r}");
        let (status, r) = p.post(&base, "/phone/api/push", json!({ "endpoint": "https://fcm.googleapis.com/fcm/send/xyz", "keys": keys }));
        assert_eq!((status, &r["subscribed"]), (200, &json!(true)), "{r}");
        let stranger = FakePhone { key: ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap()), id: "d1".into() };
        assert_eq!(stranger.post(&base, "/phone/api/push", json!({ "endpoint": null })).0, 401);

        let mut db = crate::Db::open(&path).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:test".into());
        let key = db.create_issue(&NewIssue { project: "NIT".into(), title: "Billing".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "claude".into(), allow_similar: true, ..Default::default() }, &agent).unwrap().key;
        let mut notifier = crate::push::Notifier::default();
        assert!(phone.notify_tick(&mut notifier).is_empty());
        db.create_ask("Which currency?", &["EUR".into(), "USD".into()], Some(&key), None, &agent).unwrap();
        assert!(phone.notify_tick(&mut notifier).is_empty());
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "https://fcm.googleapis.com/fcm/send/xyz");
        let message: Value = serde_json::from_slice(&crate::push::decrypt_for_tests(mine, &ua_public, &auth, &sent[0].1)).unwrap();
        assert_eq!(message["title"], "A decision waits for you");
        assert_eq!(message["body"], format!("{key} asks: Which currency?"));

        let (status, r) = p.post(&base, "/phone/api/push", json!({ "endpoint": null }));
        assert_eq!((status, &r["subscribed"]), (200, &json!(false)), "{r}");
        running.stop();
        std::fs::remove_file(store).ok();
    }

    #[test]
    fn a_paired_phone_answers_approves_and_sends_back_and_land_trusts_its_signature() {
        use crate::issues::{Actor, NewIssue};
        let app = new_secret().unwrap();
        let public = public_of(&app);
        let mut p = FakePhone { key: ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap()), id: "d1".into() };
        let device = Device { id: p.id.clone(), name: "Pixel".into(), public_key: p.public(), paired_at: now() };
        let list = sign_list(&app, vec![device.clone()]);
        let served = list.clone();
        let devices: Devices = Arc::new(move || Ok(verified(&public, &served).unwrap_or_default()));
        let path = crate::paths::unique_temp("nitely-phone", ".db");
        let db_path = path.clone();
        // Two worktrees the app sees waiting: an agent at its prompt, a stopped rebase.
        let phone = Phone::new(devices, Arc::new(|_: &str| None), Arc::new(move || crate::Db::open(&db_path)), Arc::new(|_: &crate::Db| vec!["worktree:C:/wt/x".to_string(), "worktree:C:/wt/y".to_string()]));
        let running = phone.start(&["127.0.0.1".parse().unwrap()], 0, None).unwrap();
        let base = format!("http://{}", running.addrs[0]);

        let mut db = crate::Db::open(&path).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let agent = Actor::Agent("claude:test".into());
        let new = |title: &str| NewIssue { project: "NIT".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "claude".into(), allow_similar: true, ..Default::default() };
        let waiting = |db: &mut crate::Db, title: &str, patch: &str| {
            let key = db.create_issue(&new(title), &agent).unwrap().key;
            db.move_issue(&key, "In Progress", None, None, &agent).unwrap();
            db.move_issue(&key, "Needs You", None, None, &agent).unwrap();
            db.conn
                .execute(
                    "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, ts) VALUES (?1, ?2, 'c', 'codex', 'pass', 'human', '[\"billing: a.rs\"]', ?3)",
                    rusqlite::params![key, patch, now() - 10],
                )
                .unwrap();
            key
        };
        let a = waiting(&mut db, "Billing fix", "p1");
        let b = waiting(&mut db, "Pricing page", "p2");
        let ask = db.create_ask("Which currency?", &["EUR".into(), "USD".into()], Some(&a), None, &agent).unwrap();

        let (status, d) = p.get(&base, "/phone/api/decisions", now());
        assert_eq!(status, 200, "{d}");
        let kinds: Vec<&str> = d["decisions"].as_array().unwrap().iter().map(|x| x["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["ask", "approve", "approve"], "a's open ask shows beside its approval, not instead of it: {d}");
        assert_eq!(d["at_pc"], 2, "the worktrees the app sees waiting");
        let own = db.create_issue(&NewIssue { owner: Some("human".into()), ..new("Call the bank") }, &agent).unwrap().key;
        assert_eq!(p.get_with(&base, "/phone/api/decisions", now(), "aaaaaaaaaaaaaaaa").1["at_pc"], 3, "and the person's own work");
        db.move_issue(&own, "Canceled", None, Some("done elsewhere"), &Actor::Human).unwrap();
        assert_eq!((&d["decisions"][1]["patch_id"], &d["decisions"][2]["patch_id"]), (&json!("p1"), &json!("p2")));

        // An answer, signed over what it says.
        let t = now();
        let (status, _) = p.post(&base, "/phone/api/answer", json!({ "ask": ask.id, "answer": "EUR", "time": t, "signature": p.sign(&answer_message(ask.id, "EUR", t)) }));
        assert_eq!(status, 200);
        assert_eq!(db.ask(ask.id).unwrap().unwrap().answer.as_deref(), Some("EUR"));
        let forged = p.post(&base, "/phone/api/answer", json!({ "ask": ask.id, "answer": "USD", "time": t, "signature": p.sign(&answer_message(ask.id, "EUR", t)) }));
        assert_eq!(forged.0, 400, "a signature over another answer");

        // An approval of another patch than the latest review's is refused; of the right one, it lands.
        let t = now();
        let stale = p.post(&base, "/phone/api/approve", json!({ "key": b, "patch": "old", "time": t, "signature": p.sign(&crate::approval::message(&b, "old", t)) }));
        assert_eq!(stale.0, 409);
        let (status, r) = p.post(&base, "/phone/api/approve", json!({ "key": b, "patch": "p2", "time": t, "signature": p.sign(&crate::approval::message(&b, "p2", t)) }));
        assert_eq!(status, 200, "{r}");
        assert_eq!(db.issue(&b).unwrap().unwrap().state, "Approved");
        let at = db.human_approval_at(&b).unwrap().unwrap();
        crate::approval::trust_in_tests(Some(public));
        devices_in_tests(Some(list.clone()));
        db.check_signed_approval(&b, "p2", at).unwrap();
        assert!(db.check_signed_approval(&b, "p3", at).is_err(), "another patch");
        // Removed from the list: its approvals no longer count.
        devices_in_tests(Some(sign_list(&app, vec![])));
        assert!(db.check_signed_approval(&b, "p2", at).is_err());
        // A list that the app did not sign trusts nobody.
        devices_in_tests(Some(DeviceList { devices: vec![device.clone()], signature: "00".repeat(64) }));
        assert!(db.check_signed_approval(&b, "p2", at).is_err());

        // Sending back moves a change to Todo with the reason.
        let t = now();
        let (status, _) = p.post(&base, "/phone/api/sendback", json!({ "key": a, "reason": "Use EUR only", "time": t, "signature": p.sign(&sendback_message(&a, "Use EUR only", t)) }));
        assert_eq!(status, 200);
        assert_eq!(db.issue(&a).unwrap().unwrap().state, "Todo");
        // Back in Needs You for a question, before any new submit: the old review's change is not
        // offered for approval again.
        db.move_issue(&a, "In Progress", None, None, &agent).unwrap();
        db.move_issue(&a, "Needs You", None, None, &agent).unwrap();
        db.create_ask("Round prices up?", &[], Some(&a), None, &agent).unwrap();
        // A request of its own (the phone's requests are never replayed).
        let d = p.get_with(&base, "/phone/api/decisions", now(), "bbbbbbbbbbbbbbbb").1;
        let of_a: Vec<&str> = d["decisions"].as_array().unwrap().iter().filter(|x| x["issue"]["key"] == json!(a)).map(|x| x["kind"].as_str().unwrap()).collect();
        assert_eq!(of_a, ["ask"], "{d}");
        // Submitted again, in the same second it was sent back: its new change waits for approval.
        db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, ts) VALUES (?1, 'p4', 'c', 'codex', 'pass', 'human', '[]', ?2)",
                rusqlite::params![a, now() - 30],
            )
            .unwrap();
        db.add_note(&a, "Submitted again", "submit", &agent).unwrap();
        let d = p.get_with(&base, "/phone/api/decisions", now(), "cccccccccccccccc").1;
        let of_a: Vec<&str> = d["decisions"].as_array().unwrap().iter().filter(|x| x["issue"]["key"] == json!(a)).map(|x| x["kind"].as_str().unwrap()).collect();
        assert_eq!(of_a, ["ask", "approve"], "{d}");

        // Another key under this device's id can do none of it.
        p.key = ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap());
        assert_eq!(p.get(&base, "/phone/api/decisions", now()).0, 401);
        running.stop();
        crate::approval::trust_in_tests(None);
        devices_in_tests(None);
    }

    #[test]
    fn serves_the_page_over_https_with_the_given_certificate() {
        use std::io::{Read, Write};
        let made = rcgen::generate_simple_self_signed(vec!["nitely.test".to_string()]).unwrap();
        let dir = crate::paths::unique_temp("nitely-phone-tls", "");
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("pc.crt"), dir.join("pc.key"));
        std::fs::write(&cert, made.cert.pem()).unwrap();
        std::fs::write(&key, made.key_pair.serialize_pem()).unwrap();
        assert!(tls_from_files(&cert, &key, "").is_err(), "a name is needed for the link");
        let other = rcgen::generate_simple_self_signed(vec!["other.test".to_string()]).unwrap();
        std::fs::write(dir.join("other.key"), other.key_pair.serialize_pem()).unwrap();
        assert!(tls_from_files(&cert, &dir.join("other.key"), "nitely.test").is_err(), "a key that is not the certificate's");
        let tls = tls_from_files(&cert, &key, "Nitely.Test.").unwrap();

        let assets: Assets = Arc::new(|p: &str| (p == "phone.html").then(|| (b"<html>phone</html>".to_vec(), content_type(p))));
        let phone = Phone::new(Arc::new(|| Ok(vec![])), assets, Arc::new(|| anyhow::bail!("no database in this test")), Arc::new(|_: &crate::Db| vec![]));
        let running = phone.start(&["127.0.0.1".parse().unwrap()], 0, Some(tls)).unwrap();
        let port = running.addrs[0].port();
        assert_eq!(running.pages(), [format!("https://nitely.test:{port}/phone/")]);
        let offer = phone.offer(&running.pages()).unwrap();
        assert!(offer.urls[0].starts_with(&format!("https://nitely.test:{port}/phone/#pair=")));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(made.cert.der().clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut conn = rustls::ClientConnection::new(Arc::new(config), "nitely.test".try_into().unwrap()).unwrap();
        let mut sock = TcpStream::connect(running.addrs[0]).unwrap();
        let mut stream = rustls::Stream::new(&mut conn, &mut sock);
        stream.write_all(b"GET /phone/ HTTP/1.1\r\nHost: nitely.test\r\n\r\n").unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 200") && answer.ends_with("<html>phone</html>"), "{answer}");

        // Plain HTTP to the HTTPS port gets no page.
        let mut plain = TcpStream::connect(running.addrs[0]).unwrap();
        plain.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        plain.write_all(b"GET /phone/ HTTP/1.1\r\n\r\n").unwrap();
        let mut got = Vec::new();
        let _ = plain.read_to_end(&mut got);
        assert!(!String::from_utf8_lossy(&got).contains("<html>phone</html>"));
        running.stop();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_client_trickling_tls_bytes_is_cut_off_at_the_deadline() {
        use std::io::{Read, Write};
        let made = rcgen::generate_simple_self_signed(vec!["nitely.test".to_string()]).unwrap();
        let tls = tls_from_pem(made.cert.pem().as_bytes(), made.key_pair.serialize_pem().as_bytes(), "nitely.test", "cert", "key").unwrap();
        let assets: Assets = Arc::new(|_: &str| None);
        let phone = Phone::new(Arc::new(|| Ok(vec![])), assets, Arc::new(|| anyhow::bail!("no database in this test")), Arc::new(|_: &crate::Db| vec![]));
        let running = phone.start(&["127.0.0.1".parse().unwrap()], 0, Some(tls)).unwrap();
        let mut sock = TcpStream::connect(running.addrs[0]).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_millis(50))).unwrap();
        // The start of a ClientHello record that announces far more than ever comes, a byte at a time.
        let hello = [0x16u8, 0x03, 0x01, 0x3f, 0xff, 0x01, 0x00, 0x3f, 0xfb, 0x03, 0x03];
        let started = Instant::now();
        let mut closed = false;
        for byte in hello.iter().cycle().take(200) {
            if sock.write_all(&[*byte]).is_err() {
                closed = true;
                break;
            }
            let mut buf = [0u8; 64];
            match sock.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            if started.elapsed() > http::IO_TIMEOUT + std::time::Duration::from_secs(5) {
                break;
            }
        }
        assert!(closed, "the server still held the connection after {:?}", started.elapsed());
        assert!(started.elapsed() < http::IO_TIMEOUT + std::time::Duration::from_secs(3), "{:?}", started.elapsed());
        running.stop();
    }

    #[test]
    fn the_pairing_link_is_one_the_phone_can_reach() {
        let running = |addrs: &[&str]| Running { addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(), host: None, stoppers: vec![], notifier: None };
        let r = running(&["127.0.0.1:7422", "[fd7a:115c:a1e0::1]:7422", "100.101.7.42:7422"]);
        assert_eq!(r.pages(), ["http://100.101.7.42:7422/phone/", "http://[fd7a:115c:a1e0::1]:7422/phone/", "http://127.0.0.1:7422/phone/"]);
        let r = Running { host: Some("pc.tail1234.ts.net".into()), ..running(&["127.0.0.1:7422", "192.168.1.68:7422"]) };
        assert_eq!(r.pages(), ["https://pc.tail1234.ts.net:7422/phone/"]);
    }

    #[test]
    fn reads_tailscale_status_for_the_away_from_home_card() {
        let json = r#"{
            "BackendState": "Running",
            "Self": { "HostName": "pc", "DNSName": "pc.tail1234.ts.net.", "OS": "windows", "TailscaleIPs": ["100.101.7.42", "fd7a:115c:a1e0::1"] },
            "Peer": {
                "nodekey:a": { "HostName": "Pixel-9", "OS": "android", "Online": false },
                "nodekey:b": { "HostName": "laptop", "OS": "macOS", "Online": true },
                "nodekey:c": { "HostName": "iPhone", "OS": "iOS", "Online": true }
            }
        }"#;
        let s = parse_tailscale_status(json).unwrap();
        assert!(s.installed && s.running);
        assert_eq!(s.name.as_deref(), Some("pc.tail1234.ts.net"));
        assert_eq!(s.ips, ["100.101.7.42", "fd7a:115c:a1e0::1"]);
        assert_eq!(s.phones, [TailnetPhone { name: "iPhone".into(), online: true }, TailnetPhone { name: "Pixel-9".into(), online: false }]);
        let out = parse_tailscale_status(r#"{ "BackendState": "NeedsLogin", "Self": { "DNSName": "" } }"#).unwrap();
        assert!(out.installed && !out.running && out.name.is_none());
        assert_eq!(out.backend, "NeedsLogin");
    }

    #[test]
    fn pairing_codes_tie_the_secret_to_the_phone_key() {
        let s = [7u8; 32];
        let code = pairing_code(&s, "ab");
        assert_eq!(code.len(), 9);
        assert_eq!(code, pairing_code(&s, "AB"));
        assert_ne!(code, pairing_code(&s, "ac"));
        assert_ne!(code, pairing_code(&[8u8; 32], "ab"));
    }
}
