use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use nitely_core::{db::NewEvent, hook, paths, Config, Db, RepoConfig};
use std::io::Read;

/// Nitely: what your coding agents are doing.
#[derive(Parser)]
#[command(name = "nitely", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record an agent hook event. Reads the hook's JSON on stdin and always exits 0,
    /// so a Nitely problem never blocks the agent.
    Hook {
        /// Which agent sent it. Only `claude` is supported in milestone 1.
        vendor: String,
    },
    /// Report what you are doing, from the current directory's worktree.
    Status {
        /// One line, e.g. "running test:gate".
        text: String,
        /// Session to attach this to; defaults to one per directory.
        #[arg(long)]
        session: Option<String>,
    },
    /// Manage the repos Nitely watches.
    Repo {
        #[command(subcommand)]
        command: RepoCmd,
    },
    /// Print the Now board as JSON.
    Now,
    /// Pull PRs from Bitbucket and issues from Linear, using ~/.aera-work/credentials.json.
    Sync,
    /// Print where Nitely keeps its data.
    Where,
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Watch a repo: its main checkout and all its worktrees.
    Add {
        /// Path to the main checkout.
        root: String,
        /// Branch tasks are cut from (e.g. win-dev).
        #[arg(long)]
        integration: String,
        /// Display name; defaults to the folder name.
        #[arg(long)]
        name: Option<String>,
    },
    List,
}

fn main() {
    let cli = Cli::parse();
    if let Cmd::Hook { vendor } = &cli.command {
        if let Err(e) = run_hook(vendor) {
            eprintln!("nitely hook: {e:#}");
        }
        return;
    }
    if let Err(e) = run(cli.command) {
        eprintln!("nitely: {e:#}");
        std::process::exit(1);
    }
}

fn run_hook(vendor: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let value: serde_json::Value = serde_json::from_str(&input)?;
    let event = match vendor {
        "claude" => hook::from_claude(&value)?,
        other => bail!("unsupported vendor {other}"),
    };
    Db::open_default()?.record(&event)
}

fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Hook { .. } => unreachable!(),
        Cmd::Status { text, session } => {
            let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
            let session_id = session.unwrap_or_else(|| format!("cli:{}", paths::normalize(&cwd)));
            Db::open_default()?.record(&NewEvent {
                session_id,
                vendor: "cli".into(),
                cwd,
                kind: "status".into(),
                summary: Some(text),
                state: "working".into(),
            })
        }
        Cmd::Repo { command } => {
            let mut config = Config::load()?;
            match command {
                RepoCmd::Add { root, integration, name } => {
                    let root = std::fs::canonicalize(&root)
                        .map(|p| clean_path(&p.to_string_lossy()))
                        .unwrap_or(root);
                    let name = name.unwrap_or_else(|| {
                        root.replace('\\', "/").rsplit('/').next().unwrap_or("repo").to_string()
                    });
                    config.upsert_repo(RepoConfig { name: name.clone(), root, integration_branch: integration });
                    config.save()?;
                    println!("watching {name}");
                }
                RepoCmd::List => println!("{}", serde_json::to_string_pretty(&config.repos)?),
            }
            Ok(())
        }
        Cmd::Now => {
            let views = nitely_core::snapshot(&Config::load()?, &Db::open_default()?)?;
            println!("{}", serde_json::to_string_pretty(&views)?);
            Ok(())
        }
        Cmd::Sync => {
            let report = nitely_core::remote::sync(&Config::load()?, &mut Db::open_default()?);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.credentials_found {
                eprintln!("no credentials file at {}", nitely_core::credentials::path().display());
            }
            if !report.errors.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Where => {
            println!("data:   {}", paths::data_dir().display());
            println!("db:     {}", paths::db_path().display());
            println!("config: {}", paths::config_path().display());
            println!("creds:  {}", nitely_core::credentials::path().display());
            Ok(())
        }
    }
}

/// `canonicalize` on Windows returns `\\?\C:\...`; git and hooks never use that prefix.
fn clean_path(p: &str) -> String {
    p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
}
