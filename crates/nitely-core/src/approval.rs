//! Signed approvals (docs/approvals.md). The app keeps the signing key in the OS keychain and signs
//! when the person approves. `land` verifies with the public key, which it also reads from the
//! keychain (its own entry, which holds nothing secret): a key written into Nitely's database or
//! files is never trusted.
//!
//! Every keychain entry belongs to one data folder: the person's own uses the service `Nitely`,
//! any other (`NITELY_HOME`: a test, a sandbox, a second profile) a service of its own, so a
//! process on another data folder can never read the person's key or sign with it. The test
//! harness goes further and never opens the keychain at all.

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

/// Under the test harness (src-tauri/src/harness.rs) the app signs with a key that lives only in its
/// process and never reads or writes the OS keychain. `land` still trusts only the keychain's key,
/// so a harness approval never lands anything real.
static EPHEMERAL: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();

/// Under the test harness, the other entries the app keeps in the keychain (the paired phones)
/// live here instead, so a harness app never reads or replaces the person's real ones.
static EPHEMERAL_ENTRIES: std::sync::Mutex<std::collections::BTreeMap<String, String>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Set when this process made a new approval key for a data folder that is not the person's own:
/// what was approved or paired there under the shared entries before has to be done again.
static NEW_FOLDER_KEY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the app tells the person when it made this data folder its own approval key (a profile on
/// `NITELY_HOME`), once per run; None on the person's own folder or when the key was there already.
pub fn new_folder_key_notice() -> Option<&'static str> {
    NEW_FOLDER_KEY.load(std::sync::atomic::Ordering::Relaxed).then_some(NEW_FOLDER_KEY_NOTICE)
}

const NEW_FOLDER_KEY_READ: &str = "The person read that this profile has its own approval key";

pub const NEW_FOLDER_KEY_NOTICE: &str =
    "This profile has its own approval key now: approve waiting changes and pair phones again.";

/// Called by the app when the test harness runs it: approvals are signed with a throwaway key.
pub fn use_ephemeral_key() -> Result<()> {
    let secret = new_secret()?;
    let _ = EPHEMERAL.set(secret);
    Ok(())
}

/// Called by the app at start: it made the keychain entries, so it reads them without a prompt.
pub fn keychain_is_silent() {
    SILENT_KEYCHAIN.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The keychain service for the data folder `data`: `Nitely` for the person's own (`real`), and for
/// any other `Nitely (<hash of its path>)`, which nothing on the person's own folder reads.
///
/// Deliberately no migration: a folder that used the `Nitely` entries before (a second profile on
/// `NITELY_HOME`) starts with a key of its own, so changes approved there before are approved
/// again and its phones pair again. Copying the person's key into another folder's entries is
/// exactly what a sandbox must never be able to do.
pub(crate) fn service_for(data: &std::path::Path, real: &std::path::Path) -> String {
    let folder = folder_key(data);
    if folder == folder_key(real) {
        return SERVICE.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(folder.as_bytes());
    format!("{SERVICE} ({})", hex(&digest[..8]))
}

/// One spelling per folder: links, `.` and `..` and a relative path resolved where the folder
/// exists (the app and an agent's CLI must pick the same entries for one database), then
/// `paths::normalize`.
fn folder_key(path: &std::path::Path) -> String {
    let resolved = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string_lossy().into_owned());
    // Windows hands back the verbatim form, `\\?\C:\...` or `\\?\UNC\server\...`.
    let plain = match resolved.strip_prefix(r"\\?\") {
        Some(rest) => rest.strip_prefix(r"UNC\").map_or_else(|| rest.to_string(), |unc| format!(r"\\{unc}")),
        None => resolved,
    };
    crate::paths::normalize(&plain)
}

/// The one place Nitely opens a keychain entry: in this data folder's service, and never under the
/// test harness.
fn entry(account: &str) -> Result<keyring::Entry> {
    if EPHEMERAL.get().is_some() {
        bail!("the test harness never opens the OS keychain");
    }
    let service = service_for(&crate::paths::data_dir(), &crate::paths::real_data_dir());
    keyring::Entry::new(&service, account).map_err(|e| anyhow::anyhow!("the OS keychain: {e}"))
}

/// A Nitely keychain entry's text; None when it does not exist. On macOS only the app reads
/// without the OS asking the person.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn keychain_get(account: &str) -> Result<Option<String>> {
    if EPHEMERAL.get().is_some() {
        return Ok(EPHEMERAL_ENTRIES.lock().unwrap().get(account).cloned());
    }
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
    if EPHEMERAL.get().is_some() {
        EPHEMERAL_ENTRIES.lock().unwrap().insert(account.to_string(), text.to_string());
        return Ok(());
    }
    entry(account)?.set_password(text).map_err(|e| anyhow::anyhow!("saving {account} in the OS keychain: {e}"))
}

/// The app's signing secret, made and saved in the keychain the first time. Only the app calls it.
pub fn keychain_secret() -> Result<[u8; 32]> {
    if let Some(secret) = EPHEMERAL.get() {
        return Ok(*secret);
    }
    let e = entry(SECRET_ACCOUNT)?;
    match e.get_password() {
        Ok(hex) => unhex::<32>(&hex).context("the approval key in the OS keychain is not one Nitely made"),
        Err(keyring::Error::NoEntry) => {
            let secret = new_secret()?;
            e.set_password(&hex(&secret)).map_err(|e| anyhow::anyhow!("saving the approval key in the OS keychain: {e}"))?;
            // Only this folder's own service was looked in: the person's entries are never read.
            if service_for(&crate::paths::data_dir(), &crate::paths::real_data_dir()) != SERVICE {
                NEW_FOLDER_KEY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(secret)
        }
        Err(err) => bail!("reading the approval key from the OS keychain: {err}"),
    }
}

/// Puts the public half of `secret` in the keychain, where land reads it; true when it replaced a
/// different one (someone changed it).
pub fn publish_public_key(secret: &[u8; 32]) -> Result<bool> {
    if EPHEMERAL.get().is_some() {
        return Ok(false);
    }
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

/// The public key approvals are checked with: the keychain's. Under the test harness, the throwaway
/// key's, which signed them: the harness app (its background lander too) never reads the keychain.
/// The CLI never has a throwaway key, so `nitely land` trusts only the keychain's.
pub fn trusted_key() -> Result<[u8; 32]> {
    #[cfg(test)]
    {
        return TEST_KEY.with(|k| k.get()).context("no approval key yet: open the Nitely app once so it can make one");
    }
    #[allow(unreachable_code)]
    {
        if let Some(secret) = EPHEMERAL.get() {
            return Ok(public_of(secret));
        }
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

/// The public key the app's own signed records (the paired phones) are checked with: the throwaway
/// key's under the test harness, which signed them, otherwise the keychain's.
pub fn app_key() -> Result<[u8; 32]> {
    match EPHEMERAL.get() {
        Some(secret) => Ok(public_of(secret)),
        None => trusted_key(),
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
    /// Records that the app made this data folder its own approval key (`new_folder_key_notice`).
    pub fn log_new_folder_key(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', NULL, 'approval_key', ?2)",
            params![now(), NEW_FOLDER_KEY_NOTICE],
        )?;
        Ok(())
    }

    /// When the app last made this data folder its own approval key (`log_new_folder_key`), while the
    /// person has not dismissed that notice: the app keeps saying so after a restart until then.
    /// Kept in this folder's own database, so one profile's dismissal never hides another's notice.
    pub fn new_folder_key_at(&self) -> Result<Option<i64>> {
        let made: Option<i64> =
            self.conn.query_row("SELECT MAX(id) FROM event WHERE type = 'approval_key' AND summary = ?1", params![NEW_FOLDER_KEY_NOTICE], |r| r.get(0))?;
        let read: Option<i64> =
            self.conn.query_row("SELECT MAX(id) FROM event WHERE type = 'approval_key' AND summary = ?1", params![NEW_FOLDER_KEY_READ], |r| r.get(0))?;
        let Some(made) = made.filter(|m| read.is_none_or(|r| r < *m)) else { return Ok(None) };
        Ok(Some(self.conn.query_row("SELECT ts FROM event WHERE id = ?1", params![made], |r| r.get(0))?))
    }

    /// The person dismissed the new-key notice (`new_folder_key_at`).
    pub fn dismiss_new_folder_key(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', NULL, 'approval_key', ?2)",
            params![now(), NEW_FOLDER_KEY_READ],
        )?;
        Ok(())
    }

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

    /// The only test that turns on the harness's throwaway key: it is process-wide once set.
    #[test]
    fn the_harness_keeps_its_entries_off_the_os_keychain() {
        use_ephemeral_key().unwrap();
        let secret = keychain_secret().unwrap();
        assert_eq!(app_key().unwrap(), public_of(&secret), "the phone list is checked with the key that signed it");
        assert!(!publish_public_key(&secret).unwrap());
        let account = "harness-test-entry";
        assert_eq!(keychain_get(account).unwrap(), None);
        keychain_set(account, "only in this process").unwrap();
        assert_eq!(keychain_get(account).unwrap().as_deref(), Some("only in this process"));
        assert!(entry(account).is_err(), "the harness never opens the OS keychain");
    }

    #[test]
    fn another_data_folder_has_keychain_entries_of_its_own() {
        let real = std::path::Path::new(r"C:\Users\you\AppData\Roaming\Nitely");
        assert_eq!(service_for(real, real), "Nitely", "the person's own folder keeps the entries it always had");
        assert_eq!(service_for(std::path::Path::new("C:/Users/you/AppData/Roaming/Nitely/"), real), "Nitely", "however it is spelled");
        let sandbox = service_for(std::path::Path::new(r"C:\Users\you\AppData\Local\Temp\nitely-harness-x"), real);
        let profile = service_for(std::path::Path::new(r"D:\nitely-second"), real);
        assert!(sandbox.starts_with("Nitely (") && profile.starts_with("Nitely ("), "{sandbox} {profile}");
        assert_ne!(sandbox, profile);
        assert_eq!(sandbox, service_for(std::path::Path::new(r"C:\Users\you\AppData\Local\Temp\nitely-harness-x"), real), "stable");

        // The app and an agent's CLI spell one existing folder differently and still agree.
        let dir = crate::paths::unique_temp("nitely-keychain", "");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let one = service_for(&dir, real);
        assert_eq!(service_for(&dir.join("."), real), one);
        assert_eq!(service_for(&dir.join("sub").join(".."), real), one);
        assert_eq!(service_for(&dir.join("sub"), &dir.join("sub").join(".")), "Nitely", "the person's own folder, spelled two ways");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_new_folder_key_is_recorded_until_read() {
        let dir = crate::paths::unique_temp("nitely-approval", "");
        let db = Db::open(&dir.join("t.db")).unwrap();
        assert_eq!(db.new_folder_key_at().unwrap(), None);
        db.log_approval_key_restored().unwrap();
        assert_eq!(db.new_folder_key_at().unwrap(), None, "another approval_key event is not this one");
        db.log_new_folder_key().unwrap();
        assert!(db.new_folder_key_at().unwrap().is_some());
        db.dismiss_new_folder_key().unwrap();
        assert_eq!(db.new_folder_key_at().unwrap(), None, "dismissed, in this folder's own database");
        db.log_new_folder_key().unwrap();
        assert!(db.new_folder_key_at().unwrap().is_some(), "a later new key is said again");
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every keychain entry is opened by `entry`, which picks the data folder's service: nothing
    /// else in the crate may open one, or a process on another data folder could reach the
    /// person's approval key.
    #[test]
    fn only_entry_opens_the_keychain() {
        let mut folders = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        let mut opened = Vec::new();
        while let Some(folder) = folders.pop() {
            for file in std::fs::read_dir(&folder).unwrap() {
                let path = file.unwrap().path();
                if path.is_dir() {
                    folders.push(path);
                    continue;
                }
                let text = if path.extension().is_some_and(|e| e == "rs") { std::fs::read_to_string(&path).unwrap() } else { continue };
                let n = text.matches(concat!("keyring::", "Entry::new")).count();
                if n > 0 {
                    opened.push((path.file_name().unwrap().to_string_lossy().into_owned(), n));
                }
            }
        }
        assert_eq!(opened, [("approval.rs".to_string(), 1)]);
    }
}
