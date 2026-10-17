//! The app's side of deciding from a phone (docs/phone.md): the phone server it runs on the
//! addresses the person picks, pairing offers, the person's answers to pairing requests, and the
//! device list it signs with its approval key. Only the person reaches these, through the app.

use nitely_core::phone::{self, Address, Device, OfferView, PairRequest, Phone, Running};
use serde::Serialize;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::{approval_key, db, err};

pub struct PhoneApp {
    phone: Arc<Phone>,
    running: Mutex<Option<Running>>,
    /// Held while the server runs, when the person asked to keep the PC awake for it.
    awake: Mutex<Option<nitely_core::awake::KeepAwake>>,
}

impl PhoneApp {
    pub fn new(app: tauri::AppHandle) -> PhoneApp {
        let assets: phone::Assets = Arc::new(move |path: &str| {
            if let Some(asset) = app.asset_resolver().get(path.to_string()) {
                return Some((asset.bytes, asset.mime_type));
            }
            // `tauri dev` serves the desktop page from Vite and embeds nothing; the phone page
            // then comes from the last `npm run build`.
            if cfg!(debug_assertions) {
                let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("dist").join(path);
                return std::fs::read(&file).ok().map(|bytes| (bytes, phone::content_type(path)));
            }
            None
        });
        PhoneApp { phone: Phone::new(
                Arc::new(phone::load_devices),
                assets,
                Arc::new(nitely_core::Db::open_default),
                Arc::new(|db: &nitely_core::Db| {
                    nitely_core::Config::load().ok().and_then(|c| nitely_core::snapshot(&c, db).ok()).map(|b| phone::waiting_in_worktrees(&b)).unwrap_or_default()
                }),
            ), running: Mutex::new(None), awake: Mutex::new(None) }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn awake(&self) -> std::sync::MutexGuard<'_, Option<nitely_core::awake::KeepAwake>> {
        self.awake.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The Phone screen: whether the server runs and where, the addresses it could use, the paired
/// phones, and pairing requests waiting for the person.
#[derive(Serialize)]
pub struct PhoneStatus {
    listening: Vec<String>,
    /// The server keeps this PC from sleeping.
    awake: bool,
    /// The addresses the server is bound to (an HTTPS link names the certificate, not these).
    bound: Vec<String>,
    addresses: Vec<Address>,
    devices: Vec<Device>,
    requests: Vec<PairRequest>,
    /// Why the paired phones could not be read, when they could not.
    devices_error: Option<String>,
    /// The paired phones that asked for notifications.
    notified: Vec<String>,
}

fn status(app: &PhoneApp) -> PhoneStatus {
    let (listening, bound) = app.running().as_ref().map(|r| (r.pages(), r.addrs.iter().map(|a| a.ip().to_string()).collect())).unwrap_or_default();
    let (devices, devices_error) = match phone::load_devices() {
        Ok(d) => (d, None),
        Err(e) => (vec![], Some(format!("{e:#}"))),
    };
    let notified = app.phone.push().map(|p| p.subscriptions().into_iter().map(|s| s.device).collect()).unwrap_or_default();
    PhoneStatus { listening, awake: app.awake().is_some(), bound, addresses: phone::addresses(), devices, requests: app.phone.waiting(), devices_error, notified }
}

#[tauri::command]
pub async fn phone_status(app: tauri::State<'_, PhoneApp>) -> Result<PhoneStatus, String> {
    Ok(status(&app))
}

/// How the phone server is reached: plain HTTP on loopback or the Tailscale address (`none`),
/// HTTPS with Tailscale's certificate for this machine (`tailscale`), or with the person's own
/// certificate and key files (`files`, with the name on the certificate).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TlsChoice {
    kind: String,
    #[serde(default)]
    cert: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    host: String,
}

fn tls_for(choice: Option<TlsChoice>) -> Result<Option<phone::Tls>, String> {
    let Some(c) = choice else { return Ok(None) };
    match c.kind.as_str() {
        "none" => Ok(None),
        "tailscale" => phone::tailscale_tls().map(Some).map_err(err),
        "files" => phone::tls_from_files(std::path::Path::new(c.cert.trim()), std::path::Path::new(c.key.trim()), &c.host).map(Some).map_err(err),
        other => Err(format!("{other} is not a way to serve the phone page")),
    }
}

/// Tailscale on this machine, for the Phone screen's "Away from home".
#[tauri::command]
pub async fn phone_tailscale() -> Result<phone::TailscaleState, String> {
    Ok(phone::tailscale_state())
}

/// The name Tailscale can give this machine a certificate for, or why it cannot yet.
#[tauri::command]
pub async fn phone_tailscale_name() -> Result<String, String> {
    phone::tailscale_cert_name().map_err(err)
}

/// Starts the phone server on `ips`, replacing a running one: over plain HTTP only on loopback or
/// the Tailscale address, over HTTPS (`tls`) on any of this machine's addresses.
#[tauri::command]
pub async fn phone_start(app: tauri::State<'_, PhoneApp>, ips: Vec<String>, tls: Option<TlsChoice>, keep_awake: Option<bool>) -> Result<PhoneStatus, String> {
    let ips: Vec<IpAddr> = ips.iter().map(|ip| ip.parse::<IpAddr>().map_err(|_| format!("{ip} is not an address"))).collect::<Result<_, _>>()?;
    let tls = tls_for(tls)?;
    // Notifications need the VAPID key from the keychain; without it the phone server still runs.
    if app.phone.push().is_none() {
        match nitely_core::push::Push::open_default() {
            Ok(push) => app.phone.set_push(Arc::new(push)),
            Err(e) => eprintln!("nitely: phone notifications are off: {e:#}"),
        }
    }
    if let Some(old) = app.running().take() {
        old.stop();
    }
    *app.awake() = None;
    let running = app.phone.start(&ips, phone::PORT, tls).map_err(err)?;
    *app.running() = Some(running);
    // So a phone away from home finds the PC; the display may still turn off.
    // Shown as awake only when the OS holds it.
    *app.awake() = match keep_awake.unwrap_or(false).then(nitely_core::awake::KeepAwake::new) {
        Some(Ok(held)) => Some(held),
        Some(Err(e)) => {
            eprintln!("nitely: the phone server cannot keep this PC awake: {e:#}");
            None
        }
        None => None,
    };
    Ok(status(&app))
}

#[tauri::command]
pub async fn phone_stop(app: tauri::State<'_, PhoneApp>) -> Result<PhoneStatus, String> {
    if let Some(r) = app.running().take() {
        r.stop();
    }
    *app.awake() = None;
    app.phone.cancel_offer();
    Ok(status(&app))
}

/// A new pairing offer: the QR code and links, valid for ten minutes and one phone.
#[tauri::command]
pub async fn phone_pair(app: tauri::State<'_, PhoneApp>) -> Result<OfferView, String> {
    let pages = app.running().as_ref().map(|r| r.pages()).unwrap_or_default();
    if pages.is_empty() {
        return Err("Turn the phone server on first.".into());
    }
    app.phone.offer(&pages).map_err(err)
}

/// The person's answer to a pairing request; a yes adds the phone to the list the app signs.
#[tauri::command]
pub async fn phone_decide(app: tauri::State<'_, PhoneApp>, id: String, accept: bool) -> Result<PhoneStatus, String> {
    if accept {
        // Saved first: if the keychain refuses, the request stays waiting and the person can retry.
        let device = app.phone.pending_device(&id).map_err(err)?;
        let secret = approval_key(&db()?)?;
        let mut devices = phone::load_devices().unwrap_or_default();
        devices.retain(|d| d.id != device.id);
        devices.push(device);
        phone::save_devices(&secret, devices).map_err(err)?;
    }
    if accept {
        // The device is in the list now, so the request is paired even if it expired meanwhile.
        app.phone.mark_paired(&id);
    } else {
        app.phone.decide(&id, false).map_err(err)?;
    }
    Ok(status(&app))
}

/// Removes a paired phone: its requests are refused from now on.
#[tauri::command]
pub async fn phone_revoke(app: tauri::State<'_, PhoneApp>, id: String) -> Result<PhoneStatus, String> {
    let secret = approval_key(&db()?)?;
    let mut devices = phone::load_devices().unwrap_or_default();
    devices.retain(|d| d.id != id);
    phone::save_devices(&secret, devices).map_err(err)?;
    Ok(status(&app))
}
