//! Web push to paired phones (docs/phone.md, "Notifications"): when a new decision waits, the app
//! sends each paired phone that asked for notifications a push message straight to that phone's
//! browser push service (Google's, Apple's, Mozilla's, Microsoft's). No server Nitely runs is in
//! between, and the message is encrypted to the phone (RFC 8291), so the push service cannot read
//! it. The app identifies itself to the push service with a VAPID key (RFC 8292) it keeps in the
//! OS keychain.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use ring::{aead, agreement, hkdf};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The keychain entry holding the VAPID private key (PKCS#8, base64url).
const VAPID_ACCOUNT: &str = "phone-vapid";
/// Who sends, for the push service's operators (RFC 8292 `sub`).
const SUBJECT: &str = "https://github.com/frontiermaxxing/Nitely";
/// At most one notification per phone this often; decisions that arrive meanwhile are told together.
pub const THROTTLE: Duration = Duration::from_secs(120);
/// The one record's size (RFC 8188); a message fits in one.
const RECORD_SIZE: u32 = 4096;
/// How long the push service keeps a message for a phone that is off.
const TTL_SECONDS: u32 = 24 * 3600;

/// Push services the app sends to. A subscription is the phone's to give, but the app should not
/// post to any other address a paired phone names.
const PUSH_HOSTS: &[&str] = &["fcm.googleapis.com", "android.googleapis.com", ".push.apple.com", ".push.services.mozilla.com", ".notify.windows.com"];

/// A phone's push subscription, as its browser's `PushSubscription.toJSON()` gives it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    /// The paired phone it belongs to.
    pub device: String,
    pub endpoint: String,
    /// The phone's P-256 public key, base64url.
    pub p256dh: String,
    /// The phone's 16-byte authentication secret, base64url.
    pub auth: String,
}

/// Posts one push message: the endpoint, the headers and the encrypted body; returns the push
/// service's HTTP status. Tests swap it.
pub type Sender = Arc<dyn Fn(&str, &[(&'static str, String)], &[u8]) -> Result<u16> + Send + Sync>;

pub struct Push {
    key: EcdsaKeyPair,
    /// The subscriptions, in a JSON file in the data directory.
    store: PathBuf,
    send: Sender,
    lock: Mutex<()>,
}

fn b64(text: &str) -> Result<Vec<u8>> {
    B64.decode(text.trim().trim_end_matches('=')).context("not base64url")
}

/// The app's VAPID key, made and saved in the keychain the first time. Only the app calls it.
pub fn keychain_key() -> Result<Vec<u8>> {
    if let Some(text) = crate::approval::keychain_get(VAPID_ACCOUNT)? {
        return b64(&text).context("the VAPID key in the keychain");
    }
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).map_err(|_| anyhow!("making a VAPID key"))?;
    crate::approval::keychain_set(VAPID_ACCOUNT, &B64.encode(doc.as_ref()))?;
    Ok(doc.as_ref().to_vec())
}

/// Sends with the same TLS setup as the rest of Nitely's outgoing requests.
pub fn https_sender() -> Sender {
    Arc::new(|endpoint: &str, headers: &[(&'static str, String)], body: &[u8]| {
        let mut req = crate::remote::agent().post(endpoint);
        for (name, value) in headers {
            req = req.header(*name, value);
        }
        Ok(req.send(body).with_context(|| format!("posting to {}", origin(endpoint).unwrap_or_default()))?.status().as_u16())
    })
}

/// The scheme and host of a push endpoint, the VAPID token's audience; only HTTPS to a known push
/// service.
pub fn origin(endpoint: &str) -> Result<String> {
    let rest = endpoint.strip_prefix("https://").context("a push endpoint is an https:// address")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let name = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    if name.is_empty() || host.contains('@') || endpoint.len() > 2048 || endpoint.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("that is not a push endpoint");
    }
    if !PUSH_HOSTS.iter().any(|h| if h.starts_with('.') { name.ends_with(h) } else { name == *h }) {
        bail!("{name} is not a push service Nitely sends to");
    }
    Ok(format!("https://{host}"))
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Result<Vec<u8>> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let mut out = vec![0; len];
    prk.expand(&[info], Len(len)).and_then(|okm| okm.fill(&mut out)).map_err(|_| anyhow!("HKDF"))?;
    Ok(out)
}

/// RFC 8291 with the ECDH secret, the sender's public key and the salt given: the message body,
/// one aes128gcm record (RFC 8188) with the sender's public key as its key id.
fn seal(ecdh_secret: &[u8], as_public: &[u8], salt: &[u8; 16], ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let ikm = hkdf(auth, ecdh_secret, &key_info, 32)?;
    let cek = hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf(salt, &ikm, b"Content-Encoding: nonce\0", 12)?;
    let mut record = plaintext.to_vec();
    // The last (and only) record's delimiter, no padding.
    record.push(2);
    if record.len() + 16 > RECORD_SIZE as usize {
        bail!("the notification is too long");
    }
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("AES key"))?);
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce).map_err(|_| anyhow!("nonce"))?;
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record).map_err(|_| anyhow!("encrypting"))?;
    let mut out = salt.to_vec();
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    Ok(out)
}

/// Encrypts `plaintext` to a phone's key and secret with a new one-time key of the app's.
pub fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if ua_public.len() != 65 || auth.len() != 16 {
        bail!("that is not a push subscription's key");
    }
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| anyhow!("randomness"))?;
    let mine = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).map_err(|_| anyhow!("making a one-time key"))?;
    let as_public = mine.compute_public_key().map_err(|_| anyhow!("making a one-time key"))?.as_ref().to_vec();
    let secret = agreement::agree_ephemeral(mine, &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public), |s| s.to_vec()).map_err(|_| anyhow!("that is not a P-256 key"))?;
    seal(&secret, &as_public, &salt, ua_public, auth, plaintext)
}

impl Push {
    pub fn new(pkcs8: &[u8], store: PathBuf, send: Sender) -> Result<Push> {
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &SystemRandom::new()).map_err(|e| anyhow!("the VAPID key: {e}"))?;
        Ok(Push { key, store, send, lock: Mutex::new(()) })
    }

    /// The app's push: its VAPID key from the keychain, subscriptions in the data directory.
    pub fn open_default() -> Result<Push> {
        Push::new(&keychain_key()?, crate::paths::data_dir().join("phone-push.json"), https_sender())
    }

    /// The VAPID public key the phone subscribes under (`applicationServerKey`), base64url.
    pub fn public_key(&self) -> String {
        B64.encode(self.key.public_key().as_ref())
    }

    /// The `Authorization` header for one endpoint: a VAPID token valid for 12 hours (RFC 8292).
    pub fn vapid(&self, endpoint: &str, now: i64) -> Result<String> {
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(serde_json::to_vec(&json!({ "aud": origin(endpoint)?, "exp": now + 12 * 3600, "sub": SUBJECT }))?);
        let input = format!("{header}.{claims}");
        let sig = self.key.sign(&SystemRandom::new(), input.as_bytes()).map_err(|_| anyhow!("signing the VAPID token"))?;
        Ok(format!("vapid t={input}.{}, k={}", B64.encode(sig.as_ref()), self.public_key()))
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn subscriptions(&self) -> Vec<Subscription> {
        std::fs::read(&self.store).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, subs: &[Subscription]) -> Result<()> {
        if let Some(dir) = self.store.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.store.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(subs)?)?;
        std::fs::rename(&tmp, &self.store).with_context(|| format!("saving {}", self.store.display()))
    }

    /// Saves a phone's subscription, replacing its earlier one.
    pub fn subscribe(&self, sub: Subscription) -> Result<()> {
        origin(&sub.endpoint)?;
        if b64(&sub.p256dh).map_or(true, |k| k.len() != 65) || b64(&sub.auth).map_or(true, |a| a.len() != 16) {
            bail!("that is not a push subscription's key");
        }
        let _g = self.guard();
        let mut subs = self.subscriptions();
        subs.retain(|s| s.device != sub.device);
        subs.push(sub);
        self.save(&subs)
    }

    /// Forgets a phone's subscription.
    pub fn unsubscribe(&self, device: &str) -> Result<()> {
        let _g = self.guard();
        let mut subs = self.subscriptions();
        let before = subs.len();
        subs.retain(|s| s.device != device);
        if subs.len() == before {
            return Ok(());
        }
        self.save(&subs)
    }

    /// Whether this phone asked for notifications.
    pub fn subscribed(&self, device: &str) -> bool {
        self.subscriptions().iter().any(|s| s.device == device)
    }

    /// Sends `message` to every subscribed phone among `paired`. A subscription the push service
    /// says is gone (404, 410), or whose phone is no longer paired, is forgotten; one the phone
    /// replaced meanwhile is kept.
    pub fn notify(&self, paired: &[String], message: &Value, now: i64) -> Delivery {
        self.notify_each(paired, &|_| Some(message.clone()), now)
    }

    /// `notify` with each phone's own message (`message_for`; None sends it nothing), so a phone
    /// that missed one can be told it again with its next. Each outcome is in `results`.
    pub fn notify_each(&self, paired: &[String], message_for: &dyn Fn(&Subscription) -> Option<Value>, now: i64) -> Delivery {
        let mut delivery = Delivery::default();
        let mut gone = HashSet::new();
        for sub in self.subscriptions() {
            if !paired.contains(&sub.device) {
                gone.insert((sub.device.clone(), sub.endpoint.clone()));
                delivery.results.push((sub.device.clone(), Sent::Gone));
                continue;
            }
            let Some(message) = message_for(&sub) else { continue };
            let sent = match self.send_one(&sub, message.to_string().as_bytes(), now) {
                Ok(200..=299) => {
                    delivery.delivered += 1;
                    Sent::Delivered
                }
                Ok(404 | 410) => {
                    // The subscription is gone, not the phone: it may have handed over a new one,
                    // which is owed what this one missed.
                    gone.insert((sub.device.clone(), sub.endpoint.clone()));
                    Sent::Failed
                }
                Ok(status) => {
                    delivery.failed.push(format!("{}: the push service answered {status}", sub.device));
                    Sent::Failed
                }
                Err(e) => {
                    delivery.failed.push(format!("{}: {e:#}", sub.device));
                    Sent::Failed
                }
            };
            delivery.results.push((sub.device.clone(), sent));
        }
        if !gone.is_empty() {
            let _g = self.guard();
            let mut subs = self.subscriptions();
            subs.retain(|s| !gone.contains(&(s.device.clone(), s.endpoint.clone())));
            if let Err(e) = self.save(&subs) {
                delivery.failed.push(format!("{e:#}"));
            }
        }
        delivery
    }

    fn send_one(&self, sub: &Subscription, plaintext: &[u8], now: i64) -> Result<u16> {
        let body = encrypt(&b64(&sub.p256dh)?, &b64(&sub.auth)?, plaintext)?;
        let headers = [
            ("TTL", TTL_SECONDS.to_string()),
            ("Urgency", "high".to_string()),
            ("Content-Encoding", "aes128gcm".to_string()),
            ("Content-Type", "application/octet-stream".to_string()),
            ("Authorization", self.vapid(&sub.endpoint, now)?),
        ];
        (self.send)(&sub.endpoint, &headers, &body)
    }
}

/// What one message did: how many phones' push services took it, and what failed, one line per
/// phone.
#[derive(Debug, Default)]
pub struct Delivery {
    pub delivered: usize,
    pub failed: Vec<String>,
    /// Each phone's outcome, by device id.
    pub results: Vec<(String, Sent)>,
}

/// What became of one phone's message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sent {
    Delivered,
    /// Not taken this time (the push service was down, say): told again with the next message.
    Failed,
    /// The phone was removed: what it was owed is forgotten.
    Gone,
}

/// The decisions one tick tells: those new since the last message, and how many wait in all.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub new: Vec<(String, String)>,
    pub waiting: usize,
}

/// Decides when to notify: a decision is new the first time it is seen waiting, and new decisions
/// are told at most once per `THROTTLE`, together. What already waited when watching began is not
/// told; a decision settled before its turn came is dropped. A phone whose message did not go out
/// is owed its decisions, and gets them with its next message while they still wait.
#[derive(Default)]
pub struct Notifier {
    seen: Option<HashSet<String>>,
    pending: Vec<(String, String)>,
    last: Option<Instant>,
    /// What each phone (by device id, so it follows a new subscription) was not told yet.
    owed: std::collections::HashMap<String, Vec<(String, String)>>,
}

impl Notifier {
    /// `waiting` is every decision waiting now, as (id, one line). Returns what to tell now: the
    /// new decisions, when there are any or a phone is owed some, and the throttle allows.
    pub fn tick(&mut self, now: Instant, waiting: &[(String, String)]) -> Option<Batch> {
        let ids: HashSet<String> = waiting.iter().map(|(id, _)| id.clone()).collect();
        let seen = self.seen.replace(ids.clone())?;
        self.pending.extend(waiting.iter().filter(|(id, _)| !seen.contains(id)).cloned());
        self.pending.retain(|(id, _)| ids.contains(id));
        for owed in self.owed.values_mut() {
            owed.retain(|(id, _)| ids.contains(id));
        }
        self.owed.retain(|_, owed| !owed.is_empty());
        if (self.pending.is_empty() && self.owed.is_empty()) || self.last.is_some_and(|l| now.duration_since(l) < THROTTLE) {
            return None;
        }
        self.last = Some(now);
        Some(Batch { new: std::mem::take(&mut self.pending), waiting: waiting.len() })
    }

    /// What one phone is told with `batch`: what it is owed, then the new decisions.
    fn items_for(&self, device: &str, batch: &Batch) -> Vec<(String, String)> {
        let mut items = self.owed.get(device).cloned().unwrap_or_default();
        let fresh: Vec<(String, String)> = batch.new.iter().filter(|(id, _)| !items.iter().any(|(o, _)| o == id)).cloned().collect();
        items.extend(fresh);
        items
    }

    /// The message for one phone, or None when it has nothing to be told.
    pub fn message_for(&self, device: &str, batch: &Batch) -> Option<Value> {
        let items = self.items_for(device, batch);
        if items.is_empty() {
            return None;
        }
        let title = if items.len() == 1 { "A decision waits for you".to_string() } else { format!("{} decisions wait for you", items.len()) };
        let lines: Vec<String> = items.iter().take(3).map(|(_, line)| line.chars().take(140).collect()).collect();
        Some(json!({ "title": title, "body": lines.join("\n"), "waiting": batch.waiting }))
    }

    /// Records what became of a phone's message: one that did not go out is owed again.
    pub fn settle(&mut self, device: &str, batch: &Batch, sent: Sent) {
        match sent {
            Sent::Failed => {
                let items = self.items_for(device, batch);
                self.owed.insert(device.to_string(), items);
            }
            Sent::Delivered | Sent::Gone => {
                self.owed.remove(device);
            }
        }
    }
}

/// The phone's side of RFC 8291, for tests: the sender's key from the header, the same
/// derivation, then open the one record and drop its delimiter.
#[cfg(test)]
pub(crate) fn decrypt_for_tests(phone: agreement::EphemeralPrivateKey, ua_public: &[u8], auth: &[u8], body: &[u8]) -> Vec<u8> {
    let salt: [u8; 16] = body[..16].try_into().unwrap();
    let as_public = &body[21..21 + body[20] as usize];
    let secret = agreement::agree_ephemeral(phone, &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public), |s| s.to_vec()).unwrap();
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let ikm = hkdf(auth, &secret, &key_info, 32).unwrap();
    let cek = hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16).unwrap();
    let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\0", 12).unwrap();
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
    let mut record = body[21 + as_public.len()..].to_vec();
    let plain = key.open_in_place(aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(), aead::Aad::empty(), &mut record).unwrap();
    assert_eq!(plain.last(), Some(&2), "the last record's delimiter");
    plain[..plain.len() - 1].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};

    fn key() -> Vec<u8> {
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap().as_ref().to_vec()
    }

    #[test]
    fn encrypts_as_rfc_8291_appendix_a_does() {
        let d = |s: &str| b64(s).unwrap();
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = seal(
            &d("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &d("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            &salt,
            &d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &d("BTBZMqHH6r4Tts7J_aSIgg"),
            &d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24"),
        )
        .unwrap();
        let header = d("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        assert_eq!(&body[..header.len()], &header[..]);
        assert_eq!(B64.encode(&body[header.len()..]), "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ");
    }

    #[test]
    fn a_phone_decrypts_what_it_was_sent() {
        let rng = SystemRandom::new();
        let phone = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = phone.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [9u8; 16];
        let body = encrypt(&ua_public, &auth, b"{\"title\":\"hi\"}").unwrap();
        assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), RECORD_SIZE);
        assert_eq!(decrypt_for_tests(phone, &ua_public, &auth, &body), b"{\"title\":\"hi\"}");
        assert!(encrypt(&ua_public[..64], &auth, b"x").is_err());
    }

    #[test]
    fn signs_a_vapid_token_for_the_push_service_only() {
        let push = Push::new(&key(), PathBuf::from("unused.json"), Arc::new(|_: &str, _: &[(&'static str, String)], _: &[u8]| Ok(201))).unwrap();
        let header = push.vapid("https://fcm.googleapis.com/fcm/send/abc", 1_000).unwrap();
        let (t, k) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(k, push.public_key());
        assert_eq!(b64(k).unwrap().len(), 65);
        let (input, sig) = t.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, b64(k).unwrap()).verify(input.as_bytes(), &b64(sig).unwrap()).unwrap();
        let claims: Value = serde_json::from_slice(&b64(input.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_000 + 12 * 3600);
        assert_eq!(origin("https://web.push.apple.com/QGx").unwrap(), "https://web.push.apple.com");
        assert_eq!(origin("https://wns2-by3p.notify.windows.com/w/?token=x").unwrap(), "https://wns2-by3p.notify.windows.com");
        for bad in ["http://fcm.googleapis.com/x", "https://example.com/push", "https://fcm.googleapis.com.example.com/x", "https://evil@fcm.googleapis.com/x", "https://pushapple.com/x"] {
            assert!(origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn notifies_subscribed_paired_phones_and_forgets_gone_ones() {
        let store = crate::paths::unique_temp("nitely-push", ".json");
        let sent: Arc<Mutex<Vec<(String, Vec<(&'static str, String)>)>>> = Arc::new(Mutex::new(vec![]));
        let log = sent.clone();
        let send: Sender = Arc::new(move |endpoint: &str, headers: &[(&'static str, String)], _: &[u8]| {
            log.lock().unwrap().push((endpoint.to_string(), headers.to_vec()));
            Ok(if endpoint.ends_with("/gone") { 410 } else if endpoint.ends_with("/down") { 503 } else { 201 })
        });
        let push = Push::new(&key(), store.clone(), send).unwrap();
        let rng = SystemRandom::new();
        let phone_key = || B64.encode(agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap().compute_public_key().unwrap().as_ref());
        let sub = |device: &str, path: &str| Subscription { device: device.into(), endpoint: format!("https://fcm.googleapis.com/fcm/send/{path}"), p256dh: phone_key(), auth: B64.encode([1u8; 16]) };
        push.subscribe(sub("a", "ok")).unwrap();
        push.subscribe(sub("b", "gone")).unwrap();
        push.subscribe(sub("c", "down")).unwrap();
        push.subscribe(sub("d", "ok")).unwrap();
        push.subscribe(sub("a", "ok2")).unwrap();
        assert!(push.subscribe(Subscription { endpoint: "https://example.com/x".into(), ..sub("e", "x") }).is_err());
        assert!(push.subscribe(Subscription { auth: B64.encode([1u8; 8]), ..sub("e", "x") }).is_err());
        assert_eq!(push.subscriptions().len(), 4, "one per phone");

        let delivery = push.notify(&["a".into(), "b".into(), "c".into()], &json!({ "title": "t" }), 1_000);
        let failed = delivery.failed;
        assert_eq!((delivery.delivered, failed.len()), (1, 1), "{failed:?}");
        assert!(failed[0].starts_with("c: ") && failed[0].contains("503"));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 3, "d is no longer paired");
        let to_a = sent.iter().find(|(e, _)| e.ends_with("/ok2")).expect("a's newer subscription");
        assert!(!sent.iter().any(|(e, _)| e.ends_with("/ok")), "not a's older one");
        let header = |name: &str| to_a.1.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(header("Content-Encoding"), "aes128gcm");
        assert!(header("Authorization").starts_with("vapid t="));
        let mut left: Vec<String> = push.subscriptions().into_iter().map(|s| s.device).collect();
        left.sort();
        assert_eq!(left, ["a", "c"], "the gone subscription and the unpaired phone are forgotten");
        push.unsubscribe("c").unwrap();
        assert!(!push.subscribed("c") && push.subscribed("a"));
        std::fs::remove_file(store).ok();
    }

    #[test]
    fn tells_new_decisions_once_and_at_most_every_two_minutes() {
        let mut n = Notifier::default();
        let t0 = Instant::now();
        let w = |ids: &[&str]| ids.iter().map(|i| (i.to_string(), format!("line {i}"))).collect::<Vec<_>>();
        assert_eq!(n.tick(t0, &w(&["a"])), None, "what waited when watching began");
        let batch = n.tick(t0 + Duration::from_secs(10), &w(&["a", "b"])).unwrap();
        let first = n.message_for("phone", &batch).unwrap();
        assert_eq!(first["title"], "A decision waits for you");
        assert_eq!(first["body"], "line b");
        assert_eq!(first["waiting"], 2);
        assert_eq!(n.tick(t0 + Duration::from_secs(20), &w(&["a", "b"])), None, "nothing new");
        assert_eq!(n.tick(t0 + Duration::from_secs(30), &w(&["a", "b", "c", "d"])), None, "too soon after the last");
        assert_eq!(n.tick(t0 + Duration::from_secs(40), &w(&["a", "b", "d", "e"])), None);
        let batch = n.tick(t0 + Duration::from_secs(10) + THROTTLE, &w(&["a", "d", "e"])).unwrap();
        let later = n.message_for("phone", &batch).unwrap();
        assert_eq!(later["title"], "2 decisions wait for you", "c was settled meanwhile");
        assert_eq!(later["body"], "line d\nline e");
        assert_eq!(n.tick(t0 + Duration::from_secs(400), &w(&["a", "d", "e"])), None);

        // Two phones: the one whose message did not go out is told those decisions again with its
        // next message, while they still wait; the other gets only what is new.
        let t1 = t0 + Duration::from_secs(1000);
        let batch = n.tick(t1, &w(&["a", "f", "g"])).unwrap();
        n.settle("down", &batch, Sent::Failed);
        n.settle("up", &batch, Sent::Delivered);
        assert_eq!(n.tick(t1 + Duration::from_secs(30), &w(&["a", "f", "g", "h"])), None, "still the throttle");
        let batch = n.tick(t1 + THROTTLE, &w(&["a", "g", "h"])).unwrap();
        assert_eq!(n.message_for("down", &batch).unwrap()["body"], "line g\nline h", "f was settled meanwhile");
        assert_eq!(n.message_for("up", &batch).unwrap()["body"], "line h");
        n.settle("down", &batch, Sent::Delivered);
        n.settle("up", &batch, Sent::Delivered);
        // Owed decisions alone are worth a message once the throttle allows; nothing owed, none.
        let batch = n.tick(t1 + THROTTLE * 2, &w(&["a", "g", "h"]));
        assert_eq!(batch, None);
        let batch = n.tick(t1 + THROTTLE * 3, &w(&["a", "g", "h", "i"])).unwrap();
        n.settle("down", &batch, Sent::Failed);
        let batch = n.tick(t1 + THROTTLE * 4, &w(&["a", "g", "h", "i"])).unwrap();
        assert_eq!(batch.new, vec![], "nothing new");
        assert_eq!(n.message_for("down", &batch).unwrap()["body"], "line i");
        assert_eq!(n.message_for("up", &batch), None);
    }

    #[test]
    fn a_subscription_replaced_while_the_old_one_is_refused_is_kept() {
        let store = crate::paths::unique_temp("nitely-push", ".json");
        let me: Arc<Mutex<Option<Arc<Push>>>> = Arc::new(Mutex::new(None));
        let rng = SystemRandom::new();
        let phone_key = B64.encode(agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap().compute_public_key().unwrap().as_ref());
        let fresh = Subscription { device: "a".into(), endpoint: "https://fcm.googleapis.com/fcm/send/new".into(), p256dh: phone_key.clone(), auth: B64.encode([1u8; 16]) };
        let (later, replacement) = (me.clone(), fresh.clone());
        let send: Sender = Arc::new(move |endpoint: &str, _: &[(&'static str, String)], _: &[u8]| {
            // The phone hands over a new subscription while the old one is being refused.
            if endpoint.ends_with("/old") {
                later.lock().unwrap().clone().unwrap().subscribe(replacement.clone()).unwrap();
                return Ok(410);
            }
            Ok(201)
        });
        let push = Arc::new(Push::new(&key(), store.clone(), send).unwrap());
        *me.lock().unwrap() = Some(push.clone());
        push.subscribe(Subscription { endpoint: "https://fcm.googleapis.com/fcm/send/old".into(), ..fresh.clone() }).unwrap();
        let delivery = push.notify(&["a".into()], &json!({ "title": "t" }), 1_000);
        assert_eq!(delivery.delivered, 0);
        assert_eq!(push.subscriptions(), [fresh]);
        std::fs::remove_file(store).ok();
    }
}
