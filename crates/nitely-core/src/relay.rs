//! The backlog for agents on other machines on the local network (`nitely serve`): the MCP server
//! over HTTP, open only to this machine's networks and only with a token. Tokens are named (the
//! name is the remote agent's identity, e.g. `claude@mac`) and stored only as SHA-256 hashes. A
//! token is made in the app and shown there once, to the human: no command an agent runs ever
//! prints or stores one. Nothing here can approve or answer for the human.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
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

/// Points machines whose default project is `from` at `to`, or at none when `to` is None.
/// Returns how many changed.
pub fn retarget_project(from: &str, to: Option<&str>) -> Result<usize> {
    retarget_project_in(&crate::paths::data_dir(), from, to)
}

fn retarget_project_in(dir: &Path, from: &str, to: Option<&str>) -> Result<usize> {
    let mut store = load(dir)?;
    let mut changed = 0;
    for t in store.tokens.iter_mut().filter(|t| t.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(from))) {
        t.project = to.map(str::to_string);
        changed += 1;
    }
    if changed > 0 {
        save(dir, &store)?;
    }
    Ok(changed)
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

/// This machine's networks: each interface's address and prefix length, loopback left out.
pub fn networks() -> Vec<(IpAddr, u8)> {
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
pub fn lan_addresses(networks: &[(IpAddr, u8)]) -> Vec<IpAddr> {
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

pub fn url(ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(v4) => format!("http://{v4}:{port}{path}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}{path}"),
    }
}

/// The addresses other machines can reach a server listening on `listen` at: every LAN address
/// of its family when it listens on all of them (`0.0.0.0`, `[::]`), else that one address,
/// which is none for loopback. `[::]` takes IPv4 too except on Windows, where it is IPv6 only.
pub fn reachable_addresses(listen: IpAddr, networks: &[(IpAddr, u8)]) -> Vec<IpAddr> {
    let all = lan_addresses(networks);
    match listen {
        ip if ip.is_loopback() => vec![],
        IpAddr::V4(v4) if v4.is_unspecified() => all.into_iter().filter(IpAddr::is_ipv4).collect(),
        IpAddr::V6(v6) if v6.is_unspecified() => {
            let mut ips: Vec<IpAddr> = all.iter().copied().filter(IpAddr::is_ipv6).collect();
            if !cfg!(windows) {
                ips.extend(all.into_iter().filter(IpAddr::is_ipv4));
            }
            ips
        }
        ip => vec![ip],
    }
}

/// The MCP URLs other machines would use for this one, for the address `nitely serve` listens on.
pub fn lan_urls() -> Vec<String> {
    let listen = serving_addr().unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)));
    reachable_addresses(listen.ip(), &networks()).into_iter().map(|ip| url(ip, listen.port(), MCP_PATH)).collect()
}

fn port_file() -> PathBuf {
    crate::paths::data_dir().join("relay-port")
}

/// Records the address `nitely serve` listens on, so the app's setup commands name it.
pub fn record_serving_addr(addr: SocketAddr) -> Result<()> {
    let file = port_file();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, addr.to_string()).with_context(|| format!("writing {}", file.display()))
}

/// The address `nitely serve` last listened on, if it ever ran. A file from before the address
/// was recorded holds only the port, of the default IPv4 listener.
pub fn serving_addr() -> Option<SocketAddr> {
    parse_serving_addr(&std::fs::read_to_string(port_file()).ok()?)
}

fn parse_serving_addr(text: &str) -> Option<SocketAddr> {
    let text = text.trim();
    text.parse().ok().or_else(|| text.parse::<u16>().ok().map(|port| SocketAddr::from(([0, 0, 0, 0], port))))
}

/// The port `nitely serve` last listened on, if it ever ran.
pub fn serving_port() -> Option<u16> {
    serving_addr().map(|a| a.port())
}

/// The command that adds this machine's backlog to Claude Code on another machine.
pub fn claude_command(mcp_url: &str, token: &str) -> String {
    format!("claude mcp add --transport http nitely {mcp_url} --header \"Authorization: Bearer {token}\"")
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
    fn a_project_key_change_follows_into_tokens() {
        let dir = std::env::temp_dir().join(format!("nitely-relay-retarget-{}-{}", std::process::id(), crate::db::now()));
        let token = add_token_in(&dir, "mac", Some("old")).unwrap();
        add_token_in(&dir, "box", Some("other")).unwrap();
        assert_eq!(retarget_project_in(&dir, "OLD", Some("NEW")).unwrap(), 1);
        assert_eq!(verify_in(&dir, &token).unwrap().unwrap().project.as_deref(), Some("NEW"), "the token still works");
        assert_eq!(retarget_project_in(&dir, "NEW", None).unwrap(), 1);
        assert_eq!(verify_in(&dir, &token).unwrap().unwrap().project, None);
    }

    #[test]
    fn tokens_are_shown_once_kept_hashed_and_checked() {
        let dir = std::env::temp_dir().join(format!("nitely-relay-{}-{}", std::process::id(), crate::db::now()));
        assert!(add_token_in(&dir, "Mac", None).is_err(), "names are lowercase");
        let token = add_token_in(&dir, "mac", Some("and")).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX) && token.len() == TOKEN_PREFIX.len() + 64);
        let stored = std::fs::read_to_string(store_path(&dir)).unwrap();
        assert!(!stored.contains(&token), "only the hash is stored");
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
    fn setup_urls_follow_the_listener() {
        let nets = [("192.168.1.68".parse().unwrap(), 24), ("2600:1700:59d:6010::2e".parse().unwrap(), 64), ("fe80::1".parse().unwrap(), 64)];
        let at = |listen: &str| reachable_addresses(listen.parse().unwrap(), &nets).iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(at("0.0.0.0"), ["192.168.1.68"]);
        let six = at("::");
        assert_eq!(six[0], "2600:1700:59d:6010::2e", "IPv6 first, never link-local: {six:?}");
        assert_eq!(six.len(), if cfg!(windows) { 1 } else { 2 }, "Windows listens on IPv6 only: {six:?}");
        assert_eq!(at("192.168.1.68"), ["192.168.1.68"]);
        assert!(at("127.0.0.1").is_empty() && at("::1").is_empty());
        assert_eq!(url("2600:1700:59d:6010::2e".parse().unwrap(), 7421, MCP_PATH), format!("http://[2600:1700:59d:6010::2e]:7421{MCP_PATH}"));
        // What serve records, and the bare port an older serve recorded.
        assert_eq!(parse_serving_addr("[::]:7500
"), Some("[::]:7500".parse().unwrap()));
        assert_eq!(parse_serving_addr("7421"), Some("0.0.0.0:7421".parse().unwrap()));
        assert_eq!(parse_serving_addr("junk"), None);
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
