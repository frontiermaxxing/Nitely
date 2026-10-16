//! The backlog for agents on other machines on the local network (`nitely serve`): the MCP server
//! over HTTP, open only to private addresses and only with a token. Tokens are named (the name is
//! the remote agent's identity, e.g. `claude@mac`), stored only as SHA-256 hashes, and shown once
//! when made. Nothing here can approve or answer for the human: remote agents act as agents.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// What `nitely serve` advertises over mDNS (Bonjour), and `nitely discover` browses for.
pub const SERVICE_TYPE: &str = "_nitely._tcp.local.";
pub const DEFAULT_PORT: u16 = 7421;
/// The MCP endpoint's path, also in the mDNS record.
pub const MCP_PATH: &str = "/mcp";
const TOKEN_PREFIX: &str = "nly_";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    /// Who uses it, e.g. `mac`; remote agents act as `<vendor>@<name>`.
    pub name: String,
    /// The project its agents work in unless a call names another.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    hash: String,
    pub created_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    tokens: Vec<Token>,
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join("relay-tokens.json")
}

fn load(dir: &Path) -> Result<Store> {
    let path = store_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save(dir: &Path, store: &Store) -> Result<()> {
    let path = store_path(dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Fills `buf` from the operating system's random source.
pub fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|e| anyhow::anyhow!("no randomness from the system: {e}"))
}

fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Makes a token for `name` (replacing its old one) and returns it. This is the only time it is
/// shown; only its hash is kept.
pub fn add_token(name: &str, project: Option<&str>) -> Result<String> {
    add_token_in(&crate::paths::data_dir(), name, project)
}

fn add_token_in(dir: &Path, name: &str, project: Option<&str>) -> Result<String> {
    if !valid_name(name) {
        bail!("a token name is 1 to 32 lowercase letters, digits or dashes, e.g. mac");
    }
    let mut bytes = [0u8; 32];
    random_bytes(&mut bytes)?;
    let token = format!("{TOKEN_PREFIX}{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let mut store = load(dir)?;
    store.tokens.retain(|t| t.name != name);
    store.tokens.push(Token {
        name: name.into(),
        project: project.map(|p| p.trim().to_uppercase()).filter(|p| !p.is_empty()),
        hash: hash(&token),
        created_at: crate::db::now(),
    });
    save(dir, &store)?;
    Ok(token)
}

/// Writes a new token to `<data folder>/relay-tokens/<name>.token` for the human to copy to that
/// machine, so it never appears in output an agent might capture. Returns the file's path.
pub fn write_token_file(name: &str, token: &str) -> Result<PathBuf> {
    write_token_file_in(&crate::paths::data_dir(), name, token)
}

fn write_token_file_in(dir: &Path, name: &str, token: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("not a token name: {name}");
    }
    let folder = dir.join("relay-tokens");
    std::fs::create_dir_all(&folder)?;
    let file = folder.join(format!("{name}.token"));
    std::fs::write(&file, format!("{token}\n")).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

/// The tokens, without their hashes.
pub fn list_tokens() -> Result<Vec<Token>> {
    list_tokens_in(&crate::paths::data_dir())
}

fn list_tokens_in(dir: &Path) -> Result<Vec<Token>> {
    Ok(load(dir)?.tokens.into_iter().map(|t| Token { hash: String::new(), ..t }).collect())
}

/// Removes `name`'s token; false when there was none.
pub fn remove_token(name: &str) -> Result<bool> {
    remove_token_in(&crate::paths::data_dir(), name)
}

fn remove_token_in(dir: &Path, name: &str) -> Result<bool> {
    let mut store = load(dir)?;
    let before = store.tokens.len();
    store.tokens.retain(|t| t.name != name);
    if store.tokens.len() == before {
        return Ok(false);
    }
    save(dir, &store)?;
    Ok(true)
}

/// The token a request's bearer value matches, if any. Every stored hash is compared in full, in
/// constant time, so timing says nothing about which part matched.
pub fn verify(bearer: &str) -> Result<Option<Token>> {
    verify_in(&crate::paths::data_dir(), bearer)
}

fn verify_in(dir: &Path, bearer: &str) -> Result<Option<Token>> {
    let given = hash(bearer.trim());
    let mut found = None;
    for t in load(dir)?.tokens {
        if constant_eq(given.as_bytes(), t.hash.as_bytes()) && found.is_none() {
            found = Some(Token { hash: String::new(), ..t });
        }
    }
    Ok(found)
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a request may come from `ip`, given this machine's networks (address and prefix
/// length per interface): a private or link-local address, or one on the same subnet as an
/// interface, which covers a LAN that hands out global IPv6 addresses. Anything else is refused,
/// so a forwarded port never exposes the backlog to the internet.
pub fn is_local_to(ip: IpAddr, networks: &[(IpAddr, u8)]) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    is_local(ip) || networks.iter().any(|(net, prefix)| same_subnet(ip, *net, *prefix))
}

fn same_subnet(a: IpAddr, b: IpAddr, prefix: u8) -> bool {
    let bits = |ip: IpAddr| -> (u128, u32) {
        match ip {
            IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
            IpAddr::V6(v6) => (u128::from(v6), 128),
        }
    };
    let ((x, width_a), (y, width_b)) = (bits(a), bits(b));
    // A /0 "network" would be everything; never treat it as the local one.
    if width_a != width_b || prefix == 0 || u32::from(prefix) > width_a {
        return false;
    }
    let shift = width_a - u32::from(prefix);
    (x >> shift) == (y >> shift)
}

/// Whether a request may come from `ip` on address alone: this machine, or private ranges
/// (RFC 1918 IPv4, link-local, IPv6 unique-local).
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_shown_once_kept_hashed_and_checked() {
        let dir = std::env::temp_dir().join(format!("nitely-relay-{}-{}", std::process::id(), crate::db::now()));
        assert!(add_token_in(&dir, "Mac", None).is_err(), "names are lowercase");
        let token = add_token_in(&dir, "mac", Some("and")).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX) && token.len() == TOKEN_PREFIX.len() + 64);
        let stored = std::fs::read_to_string(store_path(&dir)).unwrap();
        assert!(!stored.contains(&token), "only the hash is stored");
        let file = write_token_file_in(&dir, "mac", &token).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), token);
        assert!(write_token_file_in(&dir, "../x", &token).is_err(), "a name never leaves the folder");
        let t = verify_in(&dir, &token).unwrap().unwrap();
        assert_eq!((t.name.as_str(), t.project.as_deref()), ("mac", Some("AND")));
        assert!(verify_in(&dir, "nly_wrong").unwrap().is_none());
        // A new token for the same name replaces the old one.
        let again = add_token_in(&dir, "mac", None).unwrap();
        assert!(verify_in(&dir, &token).unwrap().is_none());
        assert!(verify_in(&dir, &again).unwrap().is_some());
        assert!(list_tokens_in(&dir).unwrap().iter().all(|t| t.hash.is_empty()));
        assert!(remove_token_in(&dir, "mac").unwrap());
        assert!(!remove_token_in(&dir, "mac").unwrap());
        assert!(verify_in(&dir, &again).unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn only_the_local_network_is_local() {
        for ip in ["127.0.0.1", "10.0.0.5", "172.16.3.4", "192.168.1.20", "169.254.1.1", "::1", "fd12::1", "fe80::1", "::ffff:192.168.1.2"] {
            assert!(is_local(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.64.0.1", "172.32.0.1", "2001:4860::8888", "::ffff:8.8.8.8"] {
            assert!(!is_local(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn a_lan_with_global_ipv6_counts_by_subnet() {
        let nets = [("2600:1700:59d:6010::2e".parse().unwrap(), 64), ("192.168.1.68".parse().unwrap(), 24)];
        let local = |ip: &str| is_local_to(ip.parse().unwrap(), &nets);
        assert!(local("2600:1700:59d:6010:aaaa::1"), "same /64 as this machine");
        assert!(local("192.168.1.20") && local("::ffff:192.168.1.20"));
        assert!(!local("2600:1700:59d:6011::1"), "the next /64 is not this LAN");
        assert!(!local("8.8.8.8"));
        assert!(!is_local_to("8.8.8.8".parse().unwrap(), &[("8.8.0.0".parse().unwrap(), 0)]), "a /0 is never local");
    }
}
