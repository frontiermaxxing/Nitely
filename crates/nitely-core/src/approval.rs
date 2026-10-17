//! Signed approvals (docs/approvals.md). The app keeps the signing key in the OS keychain and signs
//! when the person approves. `land` verifies with the public key, which it also reads from the
//! keychain (its own entry, which holds nothing secret): a key written into Nitely's database or
//! files is never trusted.

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rusqlite::params;

use crate::db::{now, Db};

const SERVICE: &str = "Nitely";

/// How far a phone's own time in an approval it signed may be from when the approval was recorded.
pub const PHONE_SKEW: i64 = 120;

/// In the error for an approval without a valid signature; land sends such an issue back to Needs You.
pub const UNSIGNED: &str = "is not signed by this machine's Nitely app";
const SECRET_ACCOUNT: &str = "approval-key";
const PUBLIC_ACCOUNT: &str = "approval-public-key";

/// Whether this process may read the keychain without the OS asking the person. True everywhere
/// but macOS, where only the app that made the entries reads them silently; the app sets it.
static SILENT_KEYCHAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(!cfg!(target_os = "macos"));

/// Whether an agent's process can check approvals: everywhere but macOS, where only the app reads
/// its keychain entries without the OS asking, so the app lands approved changes there itself.
pub fn agents_can_verify() -> bool {
    !cfg!(target_os = "macos")
}

/// Called by the app at start: it made the keychain entries, so it reads them without a prompt.
pub fn keychain_is_silent() {
    SILENT_KEYCHAIN.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).map_err(|e| anyhow::anyhow!("the OS keychain: {e}"))
}

/// A Nitely keychain entry's text; None when it does not exist. On macOS only the app reads
/// without the OS asking the person.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn keychain_get(account: &str) -> Result<Option<String>> {
    if !SILENT_KEYCHAIN.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("on macOS only the Nitely app reads its keychain entries");
    }
    match entry(account)?.get_password() {
        Ok(text) => Ok(Some(text)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => bail!("reading {account} from the OS keychain: {e}"),
    }
}

/// Writes a Nitely keychain entry. Only the app calls it.
pub(crate) fn keychain_set(account: &str, text: &str) -> Result<()> {
    entry(account)?.set_password(text).map_err(|e| anyhow::anyhow!("saving {account} in the OS keychain: {e}"))
}

/// The app's signing secret, made and saved in the keychain the first time. Only the app calls it.
pub fn keychain_secret() -> Result<[u8; 32]> {
    let e = entry(SECRET_ACCOUNT)?;
    match e.get_password() {
        Ok(hex) => unhex::<32>(&hex).context("the approval key in the OS keychain is not one Nitely made"),
        Err(keyring::Error::NoEntry) => {
            let secret = new_secret()?;
            e.set_password(&hex(&secret)).map_err(|e| anyhow::anyhow!("saving the approval key in the OS keychain: {e}"))?;
            Ok(secret)
        }
        Err(err) => bail!("reading the approval key from the OS keychain: {err}"),
    }
}

/// Puts the public half of `secret` in the keychain, where land reads it; true when it replaced a
/// different one (someone changed it).
pub fn publish_public_key(secret: &[u8; 32]) -> Result<bool> {
    let public = hex(&public_of(secret));
    let e = entry(PUBLIC_ACCOUNT)?;
    let was = e.get_password().ok();
    if was.as_deref() == Some(public.as_str()) {
        return Ok(false);
    }
    e.set_password(&public).map_err(|e| anyhow::anyhow!("saving the approval key's public half in the OS keychain: {e}"))?;
    Ok(was.is_some())
}

#[cfg(test)]
thread_local! {
    static TEST_KEY: std::cell::Cell<Option<[u8; 32]>> = const { std::cell::Cell::new(None) };
}

/// In tests, the key `trusted_key` returns on this thread, instead of reading the keychain.
#[cfg(test)]
pub fn trust_in_tests(public: Option<[u8; 32]>) {
    TEST_KEY.with(|k| k.set(public));
}

/// The public key approvals are checked with: the keychain's.
pub fn trusted_key() -> Result<[u8; 32]> {
    #[cfg(test)]
    {
        return TEST_KEY.with(|k| k.get()).context("no approval key yet: open the Nitely app once so it can make one");
    }
    #[allow(unreachable_code)]
    {
        if !SILENT_KEYCHAIN.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("on macOS only the Nitely app reads its approval key, so the app lands a change that waits for approval once the person approves it there");
        }
        match entry(PUBLIC_ACCOUNT)?.get_password() {
            Ok(hex) => unhex::<32>(&hex).context("the approval key's public half in the OS keychain is not one Nitely made"),
            Err(keyring::Error::NoEntry) => bail!("no approval key yet: open the Nitely app once so it can make one"),
            Err(e) => bail!("reading the approval key from the OS keychain: {e}"),
        }
    }
}

/// What is signed for an approval of `key`'s change `patch`, given at `ts`.
pub fn message(key: &str, patch: &str, ts: i64) -> String {
    format!("nitely-approval-v1\n{}\n{patch}\n{ts}", key.to_uppercase())
}

/// A new signing secret, from the OS's randomness.
pub fn new_secret() -> Result<[u8; 32]> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("no randomness for the approval key: {e}"))?;
    Ok(secret)
}

pub fn public_of(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

pub fn sign(secret: &[u8; 32], message: &str) -> String {
    hex(&SigningKey::from_bytes(secret).sign(message.as_bytes()).to_bytes())
}

pub fn verify(public: &[u8; 32], message: &str, signature: &str) -> bool {
    let (Some(sig), Ok(key)) = (unhex::<64>(signature), VerifyingKey::from_bytes(public)) else { return false };
    key.verify(message.as_bytes(), &Signature::from_bytes(&sig)).is_ok()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl Db {
    /// Logs that the keychain's public key had been changed and the app put its own back.
    pub fn log_approval_key_restored(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', NULL, 'approval_key', ?2)",
            params![now(), "The approval key's public half in the OS keychain had been changed; the app put its own back"],
        )?;
        Ok(())
    }

    /// Signs the approval the person just gave `key` in the app: its latest review's patch, at the
    /// time of the approval. Call it right after moving the issue to Approved as the human.
    pub fn sign_approval(&self, key: &str, secret: &[u8; 32]) -> Result<()> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        let at = self.human_approval_at(&issue.key)?.with_context(|| format!("{} is not approved by the human", issue.key))?;
        let patch = self.reviews(&issue.key)?.into_iter().next().map(|r| r.patch_id).unwrap_or_default();
        let signature = sign(secret, &message(&issue.key, &patch, at));
        self.conn.execute(
            "INSERT INTO approval (issue_key, patch_id, by, ts, signature) VALUES (?1, ?2, 'human', ?3, ?4)",
            params![issue.key, patch, at, signature],
        )?;
        Ok(())
    }

    /// Sends each Approved issue whose approval carries no valid signature back to Needs You, so the
    /// person approves it again in the app: approvals given before they were signed, or written
    /// into the database. The app runs it on start, after making its key; returns the keys moved.
    pub fn unsigned_approvals_back_to_needs_you(&mut self, public: &[u8; 32]) -> Result<Vec<String>> {
        let approved: Vec<String> = self.list_issues(None, false)?.into_iter().filter(|i| i.origin == "nitely" && i.state == "Approved").map(|i| i.key).collect();
        let mut moved = Vec::new();
        for key in approved {
            let Some(at) = self.human_approval_at(&key)? else { continue };
            let patch = self.reviews(&key)?.into_iter().next().map(|r| r.patch_id).unwrap_or_default();
            if self.signature_verifies(&key, &patch, at, public)? {
                continue;
            }
            let actor = crate::issues::Actor::Agent("nitely".into());
            self.move_issue(&key, "Needs You", None, None, &actor)?;
            self.add_note(&key, "Its approval was not signed by this machine's Nitely app (given before approvals were signed, or not given in the app); approve it again in the app.", "note", &actor)?;
            moved.push(key);
        }
        Ok(moved)
    }

    /// Whether `key`'s approval at `at` of `patch` carries a signature that counts: the app's own,
    /// or a paired phone's (docs/phone.md) when that phone is still in the device list the app
    /// signed with `public` and signed within PHONE_SKEW of the approval.
    fn signature_verifies(&self, key: &str, patch: &str, at: i64, public: &[u8; 32]) -> Result<bool> {
        let mut stmt = self.conn.prepare("SELECT signature, device, signed_at FROM approval WHERE issue_key = ?1 COLLATE NOCASE AND patch_id = ?2 AND ts = ?3")?;
        let rows: Vec<(String, String, Option<i64>)> = stmt.query_map(params![key, patch, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut devices: Option<Vec<crate::phone::Device>> = None;
        for (signature, device, signed_at) in rows {
            if device.is_empty() {
                if verify(public, &message(key, patch, at), &signature) {
                    return Ok(true);
                }
                continue;
            }
            let Some(signed_at) = signed_at.filter(|t| (t - at).abs() <= PHONE_SKEW) else { continue };
            // Read once, and only when a phone signed: a list that does not verify trusts nobody.
            let list = devices.get_or_insert_with(|| crate::phone::trusted_devices(public).unwrap_or_default());
            let Some(public_key) = list.iter().find(|d| d.id == device).and_then(|d| unhex::<32>(&d.public_key)) else { continue };
            if verify(&public_key, &message(key, patch, signed_at), &signature) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Records an approval a paired phone signed (docs/phone.md): the human's approval of `key` at
    /// `at`, of `patch`, signed by `device` at the phone's own time `signed_at`.
    pub fn record_phone_approval(&self, key: &str, patch: &str, at: i64, device: &str, signed_at: i64, signature: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO approval (issue_key, patch_id, by, ts, signature, device, signed_at) VALUES (?1, ?2, 'human', ?3, ?4, ?5, ?6)",
            params![key.to_uppercase(), patch, at, signature, device, signed_at],
        )?;
        Ok(())
    }

    /// Whether `key`'s approval at `at` has no signature row at all yet: the app moves the issue to
    /// Approved and then signs, so the background lander leaves a just-given approval for a moment.
    pub fn approval_unsigned_yet(&self, key: &str, at: i64) -> Result<bool> {
        Ok(!self.conn.query_row("SELECT EXISTS (SELECT 1 FROM approval WHERE issue_key = ?1 COLLATE NOCASE AND ts = ?2)", params![key, at], |r| r.get::<_, bool>(0))?)
    }

    /// Refuses unless the human's approval of `key` at `at` is signed, for the change `patch`, by
    /// the app's approval key.
    pub fn check_signed_approval(&self, key: &str, patch: &str, at: i64) -> Result<()> {
        let public = trusted_key()?;
        if !self.signature_verifies(key, patch, at, &public)? {
            bail!("the approval of {key} {UNSIGNED} (docs/approvals.md); it is back in Needs You: approve it again in the app");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_only_the_same_approval() {
        let secret = new_secret().unwrap();
        let public = public_of(&secret);
        let sig = sign(&secret, &message("nit-3", "abc", 100));
        assert!(verify(&public, &message("NIT-3", "abc", 100), &sig));
        assert!(!verify(&public, &message("NIT-3", "abd", 100), &sig), "another patch");
        assert!(!verify(&public, &message("NIT-3", "abc", 101), &sig), "another time");
        assert!(!verify(&public_of(&new_secret().unwrap()), &message("NIT-3", "abc", 100), &sig), "another key");
        assert!(!verify(&public, &message("NIT-3", "abc", 100), "zz"), "not a signature");
        assert_eq!(unhex::<32>(&hex(&public)), Some(public));
    }
}
