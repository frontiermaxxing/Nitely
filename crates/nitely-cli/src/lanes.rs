//! `nitely run` and `nitely lanes`: build lanes from the command line (docs/build-lanes.md).

use anyhow::{Context, Result};
use clap::Subcommand;
use nitely_core::lanes;
use nitely_core::playbook::{self, Playbook};
use nitely_core::{resolve_project, Actor, Config, Db};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{cwd, print_json};

#[derive(Subcommand)]
pub enum LanesCmd {
    /// The repo's lanes, what waits and runs in them, and the last runs (the default).
    Status {
        #[arg(long)]
        project: Option<String>,
    },
    /// Put every free lane back to a clean checkout of the commit it holds; build output stays.
    Reset {
        #[arg(long)]
        project: Option<String>,
    },
    /// One run's record, then its log.
    Show { id: i64 },
}

/// The repo a project's lanes belong to, and the project's key.
fn repo_of(config: &Config, db: &Db, project: Option<&str>) -> Result<(std::path::PathBuf, String, Playbook)> {
    let dir = cwd()?;
    let project = resolve_project(config, db, project, &dir)?;
    let root = playbook::repo_for_issue(config, Some(&project), &dir)?;
    let pb = Playbook::load_with(&root, config)?;
    Ok((root, project, pb))
}

pub fn lanes(cmd: Option<LanesCmd>) -> Result<()> {
    let config = Config::load()?;
    let db = Db::open_default()?;
    match cmd.unwrap_or(LanesCmd::Status { project: None }) {
        LanesCmd::Status { project } => {
            let (root, project, pb) = repo_of(&config, &db, project.as_deref())?;
            match lanes::status(&db, &root, &pb, &project)? {
                Some(s) => print_json(&s),
                None => {
                    eprintln!("{} has no [lanes] in its playbook, so it has no build lanes (docs/build-lanes.md)", root.display());
                    Ok(())
                }
            }
        }
        LanesCmd::Reset { project } => {
            let (root, _, pb) = repo_of(&config, &db, project.as_deref())?;
            for line in lanes::reset(&root, &pb)? {
                println!("{line}");
            }
            Ok(())
        }
        LanesCmd::Show { id } => {
            let run = db.lane_run(id)?.with_context(|| format!("no run {id}"))?;
            print_json(&run)?;
            if let Some(log) = &run.log {
                if let Ok(text) = std::fs::read_to_string(log) {
                    println!("\n--- {log}\n{text}");
                }
            }
            Ok(())
        }
    }
}

/// `nitely run -- <command>`: the current worktree as a snapshot, in a lane, its output followed as
/// it comes. Exits with the command's own exit code.
pub fn run(issue: Option<String>, command: Vec<String>, me: &Actor) -> Result<()> {
    let config = Config::load()?;
    let db = Db::open_default()?;
    let command = command.join(" ");
    let stop = Arc::new(AtomicBool::new(false));
    let mut follower: Option<std::thread::JoinHandle<()>> = None;
    let result = lanes::run_in(&config, &db, &cwd()?, issue.as_deref(), &command, me, &mut |r| match r.state.as_str() {
        "queued" => eprintln!("run {} queued", r.id),
        "running" => {
            eprintln!("run {} in lane {}", r.id, r.lane.unwrap_or(0));
            if let Some(log) = r.log.clone() {
                let stop = stop.clone();
                follower = Some(std::thread::spawn(move || follow(&log, &stop)));
            }
        }
        _ => {}
    });
    stop.store(true, Ordering::SeqCst);
    if let Some(f) = follower {
        f.join().ok();
    }
    let run = result?;
    match (&run.error, run.exit_code, run.timed_out) {
        (Some(e), _, _) => {
            eprintln!("run {}: the lane could not run it: {e}", run.id);
            std::process::exit(2);
        }
        (None, _, true) => {
            eprintln!("run {}: stopped, past the lane's timeout", run.id);
            std::process::exit(124);
        }
        (None, code, _) => {
            let code = code.unwrap_or(1);
            eprintln!("run {}: exit {code}", run.id);
            std::process::exit(code);
        }
    }
}

/// Prints what `log` gains until `stop` is set, then the rest.
fn follow(log: &str, stop: &AtomicBool) {
    let mut at = 0u64;
    loop {
        let done = stop.load(Ordering::SeqCst);
        if let Ok(mut f) = std::fs::File::open(log) {
            if f.seek(SeekFrom::Start(at)).is_ok() {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    at += buf.len() as u64;
                    std::io::stdout().write_all(&buf).ok();
                    std::io::stdout().flush().ok();
                }
            }
        }
        if done {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}
