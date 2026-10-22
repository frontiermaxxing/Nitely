//! `nitely-leaderboard`: runs the Leaderboard service. See README.md.

use std::net::TcpListener;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

const USAGE: &str = "usage: nitely-leaderboard [--listen ADDR] [--db FILE] [--base PATH]

  --listen ADDR  where to listen (default 127.0.0.1:8787; NITELY_LEADERBOARD_LISTEN)
  --db FILE      the SQLite file (default leaderboard.db; NITELY_LEADERBOARD_DB)
  --base PATH    a path prefix the proxy leaves on, like /leaderboard (NITELY_LEADERBOARD_BASE)
  --site URL     where people's pages (/u/<id>) are public, for their links and cards
                 (default https://nitely.net; NITELY_LEADERBOARD_SITE)";

fn main() -> Result<()> {
    let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let mut listen = env("NITELY_LEADERBOARD_LISTEN", "127.0.0.1:8787");
    let mut db = env("NITELY_LEADERBOARD_DB", "leaderboard.db");
    let mut base = env("NITELY_LEADERBOARD_BASE", "");
    let mut site = env("NITELY_LEADERBOARD_SITE", nitely_leaderboard::SITE);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value\n\n{USAGE}"));
        match arg.as_str() {
            "--listen" => listen = value()?,
            "--db" => db = value()?,
            "--base" => base = value()?,
            "--site" => site = value()?,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown argument {arg}\n\n{USAGE}"),
        }
    }
    let base = base.trim_end_matches('/').to_string();
    let site = site.trim_end_matches('/').to_string();
    if !site.starts_with("https://") && !site.starts_with("http://127.0.0.1") && !site.starts_with("http://localhost") {
        bail!("--site is an https address, like https://example.com\n\n{USAGE}");
    }
    let store = Arc::new(nitely_leaderboard::Store::open(std::path::Path::new(&db))?);
    let listener = TcpListener::bind(&listen).with_context(|| format!("listening on {listen}"))?;
    // The one line it prints: where it listens. It never prints or keeps a client's address.
    println!("nitely-leaderboard on {listen}, base {:?}, data {db}", if base.is_empty() { "/" } else { &base });
    nitely_leaderboard::serve_site(listener, store, base, site);
    Ok(())
}
