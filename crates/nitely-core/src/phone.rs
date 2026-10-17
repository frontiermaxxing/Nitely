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
    let candidates = ["tailscale", r"C:\Program Files\Tailscale\tailscale.exe", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
    for program in candidates {
        let Ok(out) = crate::playbook::shell_free_command(program).arg("ip").stdin(std::process::Stdio::null()).output() else { continue };
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).filter(|ip| in_tailscale_range(*ip)).collect();
        }
    }
    vec![]
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
    let Some(text) = crate::approval::keychain_get(DEVICES_ACCOUNT)? else { return Ok(vec![]) };
    let list: DeviceList = serde_json::from_str(&text).context("the phone list in the OS keychain is not one Nitely wrote")?;
    let public = crate::approval::trusted_key()?;
    verified(&public, &list).context("the phone list in the OS keychain is not signed by this machine's Nitely app; pair your phone again")
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

/// The message a phone signs for each request after pairing.
pub fn request_message(method: &str, path: &str, time: i64, body: &[u8]) -> String {
    format!("nitely-phone-v1\n{method}\n{path}\n{time}\n{}", hex(&Sha256::digest(body)))
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
}

/// A running phone server; dropping it does not stop it, `stop` does.
pub struct Running {
    pub addrs: Vec<SocketAddr>,
    stoppers: Vec<Stopper>,
}

impl Running {
    pub fn stop(&self) {
        for s in &self.stoppers {
            s.stop();
        }
    }
}

impl Phone {
    pub fn new(devices: Devices, assets: Assets) -> Arc<Phone> {
        Arc::new(Phone { state: Mutex::new(State::default()), devices, assets })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Listens on each of `ips` (each must be loopback or a tunnel address) at `port`.
    pub fn start(self: &Arc<Phone>, ips: &[IpAddr], port: u16) -> Result<Running> {
        if ips.is_empty() {
            bail!("pick at least one address to listen on");
        }
        let tailscale = if ips.iter().all(|ip| ip.is_loopback()) { vec![] } else { tailscale_ips() };
        if let Some(ip) = ips.iter().find(|ip| path_kind(**ip, &tailscale).is_none()) {
            bail!("{ip} is not this machine's Tailscale address, and plain HTTP on any other network could be changed by anyone who can intercept traffic; pick the Tailscale address (docs/phone.md)");
        }
        let mut running = Running { addrs: vec![], stoppers: vec![] };
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
            let handler: http::Handler = Arc::new(move |stream: &mut TcpStream, _peer: SocketAddr| me.connection(stream));
            // Loopback answers only this machine; a tunnel address only the tunnel's peers.
            let own = *ip;
            let allow = Arc::new(move |peer: IpAddr| if own.is_loopback() { peer.is_loopback() } else { in_tailscale_range(peer) });
            let stop = flag.clone();
            std::thread::spawn(move || http::accept_until(listener, allow, Arc::new(Slots::default()), handler, stop));
            running.addrs.push(addr);
            running.stoppers.push(Stopper { addr, flag });
        }
        Ok(running)
    }

    /// A new pairing offer, replacing any open one; the secret is only in what this returns.
    pub fn offer(&self, addrs: &[SocketAddr]) -> Result<OfferView> {
        let (secret, secret_hex) = random_hex::<32>()?;
        let expires = now() + OFFER_SECONDS;
        self.state().offer = Some(Offer { secret, expires });
        let urls: Vec<String> = addrs.iter().map(|a| format!("{}#pair={secret_hex}", url(a.ip(), a.port()))).collect();
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

    fn connection(&self, stream: &mut TcpStream) {
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
            ("POST", "/phone/api/pair") => self.pair(body),
            ("GET", "/phone/api/pair/status") => self.pair_status(head, body),
            ("GET", "/phone/api/hello") => match self.authorize(head, body) {
                Ok(device) => (200, Reply::Json(json!({ "name": device.name, "id": device.id }))),
                Err((status, why)) => (status, error(why)),
            },
            _ => (404, error("Not found.")),
        }
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
        let t = now();
        if (time - t).abs() > SKEW {
            return Err((401, "The phone's clock is more than a minute off the PC's; set both to the network time."));
        }
        let public = unhex::<32>(public_key_hex).ok_or((401, "Bad key."))?;
        let ok = match (unhex::<64>(signature), VerifyingKey::from_bytes(&public)) {
            (Some(sig), Ok(key)) => key.verify(request_message(&head.method, &head.path, time, body).as_bytes(), &Signature::from_bytes(&sig)).is_ok(),
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
        "json" | "webmanifest" => "application/json",
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
        let phone = Phone::new(Arc::new(|| Ok(vec![])), Arc::new(|_: &str| None));
        let err = phone.start(&[ip("192.168.1.20")], 0).err().unwrap().to_string();
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
            let sig = self.sign(&request_message("GET", path, time, b""));
            let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
            let mut res = agent
                .get(&format!("{base}{path}"))
                .header("X-Nitely-Device", &self.id)
                .header("X-Nitely-Time", &time.to_string())
                .header("X-Nitely-Signature", &sig)
                .call()
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
        let phone = Phone::new(devices, assets);
        let running = phone.start(&["127.0.0.1".parse().unwrap()], 0).unwrap();
        let base = format!("http://{}", running.addrs[0]);

        let agent = ureq::Agent::config_builder().http_status_as_error(false).build().new_agent();
        let mut page = agent.get(&format!("{base}/phone/")).call().unwrap();
        assert_eq!(page.status().as_u16(), 200);
        assert!(page.headers().get("content-security-policy").is_some());
        assert_eq!(page.body_mut().read_to_string().unwrap(), "<html>phone</html>");

        let offer = phone.offer(&running.addrs).unwrap();
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
        assert_eq!(p.get(&base, "/phone/api/hello", t - 300).0, 401, "a request from five minutes ago");
        let stranger = FakePhone { key: ed25519_dalek::SigningKey::from_bytes(&new_secret().unwrap()), id: p.id.clone() };
        assert_eq!(stranger.get(&base, "/phone/api/hello", t + 4).0, 401, "another key under the paired id");

        // Revoked: the list no longer holds it.
        *store.lock().unwrap() = Some(sign_list(&app, vec![]));
        assert_eq!(p.get(&base, "/phone/api/hello", t + 5).0, 401);
        running.stop();
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
