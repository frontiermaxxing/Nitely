//! How long the token scan takes on a big agents' home, cold and warm (NIT-628). Run it in a build
//! lane, never on the person's own home:
//!
//!   cargo run --release -p nitely-crew --example usage_scan_bench -- make <dir> <gigabytes>
//!   cargo run --release -p nitely-crew --example usage_scan_bench -- scan <dir>   (cold: no database yet)
//!   cargo run --release -p nitely-crew --example usage_scan_bench -- scan <dir>   (warm: a new process)
//!
//! `make` writes Claude transcripts and Codex logs shaped as the agents write them: mostly prompts
//! and tool output, a usage line per response. Each `scan` is its own process, as each app start is.

use std::io::Write;
use std::path::Path;

const ROOT: &str = "/bench/app";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["make", dir, gb] => make(Path::new(dir), gb.parse()?),
        ["scan", dir] => scan(Path::new(dir)),
        // All of it: make, then a cold scan and two warm ones, each its own process, then clean up.
        ["bench", dir, gb] => {
            let dir = Path::new(dir);
            std::fs::remove_dir_all(dir).ok();
            make(dir, gb.parse()?)?;
            for _ in 0..3 {
                let status = std::process::Command::new(std::env::current_exe()?).arg("scan").arg(dir).status()?;
                anyhow::ensure!(status.success(), "a scan failed");
            }
            println!("database: {:.1} MB", std::fs::metadata(dir.join("nitely.db"))?.len() as f64 / 1e6);
            std::fs::remove_dir_all(dir).ok();
            Ok(())
        }
        _ => anyhow::bail!("usage: make <dir> <gigabytes> | scan <dir> | bench <dir> <gigabytes>"),
    }
}

fn make(dir: &Path, gb: f64) -> anyhow::Result<()> {
    let folder: String = ROOT.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let claude = dir.join("claude").join("projects").join(folder);
    let codex = dir.join("codex").join("sessions").join("2026").join("10").join("21");
    std::fs::create_dir_all(&claude)?;
    std::fs::create_dir_all(&codex)?;
    let filler = "x".repeat(4_000);
    let target = (gb * 1e9) as u64;
    let (mut written, mut file) = (0u64, 0usize);
    while written < target {
        file += 1;
        let codex_file = file % 5 == 0;
        let path = if codex_file { codex.join(format!("rollout-{file}.jsonl")) } else { claude.join(format!("s{file}.jsonl")) };
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
        if codex_file {
            writeln!(out, r#"{{"type":"session_meta","payload":{{"id":"c{file}","cwd":"{ROOT}","git":{{"branch":"codex/app-{file}-x"}}}}}}"#)?;
            writeln!(out, r#"{{"type":"turn_context","payload":{{"model":"gpt-6"}}}}"#)?;
        }
        for n in 0..2_000u64 {
            // A tool result (most of a transcript's bytes), then the response's usage.
            let line = format!(r#"{{"type":"user","message":{{"role":"user","content":"{filler}"}}}}"#);
            writeln!(out, "{line}")?;
            let usage = if codex_file {
                format!(r#"{{"timestamp":"2026-10-21T10:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{}}}}}}}}}"#, (n + 1) * 1000, (n + 1) * 400, (n + 1) * 50)
            } else {
                format!(r#"{{"type":"assistant","timestamp":"2026-10-21T10:00:00Z","sessionId":"s{file}","cwd":"{ROOT}","gitBranch":"claude/app-{file}-x","message":{{"id":"m{file}-{n}","model":"claude-opus-5-5","content":[],"usage":{{"input_tokens":5,"output_tokens":50,"cache_read_input_tokens":1000,"cache_creation_input_tokens":10}}}}}}"#)
            };
            writeln!(out, "{usage}")?;
            written += (line.len() + usage.len() + 2) as u64;
        }
    }
    println!("wrote {file} files, {:.2} GB, in {}", written as f64 / 1e9, dir.display());
    Ok(())
}

fn scan(dir: &Path) -> anyhow::Result<()> {
    // The agents' folders, as Claude Code and Codex name them in their own variables.
    std::env::set_var("CLAUDE_CONFIG_DIR", dir.join("claude"));
    std::env::set_var("CODEX_HOME", dir.join("codex"));
    let db_path = dir.join("nitely.db");
    let cold = !db_path.exists();
    let db = nitely_core::Db::open(&db_path)?;
    let config = nitely_core::Config {
        repos: vec![nitely_core::RepoConfig { name: "app".into(), root: ROOT.into(), integration_branch: "main".into(), project: Some("APP".into()) }],
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let records = nitely_crew::usage::scan(&config, &db, 0);
    let tokens: u64 = records.iter().map(|r| r.usage.tokens()).sum();
    println!("{} scan: {:.2} s, {} records, {} tokens", if cold { "cold" } else { "warm" }, started.elapsed().as_secs_f64(), records.len(), tokens);
    Ok(())
}
