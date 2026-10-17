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
    let command = shell_join(&command);
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

/// The command line the lane's shell runs. One argument is a command line already, shell syntax
/// and all (`nitely run -- "npm test && npm run build"`). Several are words, each passed on as the
/// one word it was: quoted for the shell where it has spaces, quotes or shell characters.
fn shell_join(args: &[String]) -> String {
    if let [one] = args {
        return one.clone();
    }
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

#[cfg(windows)]
fn quote(word: &str) -> String {
    // cmd passes a quoted word through, and the program's argument parser (the Windows rule:
    // backslashes before a quote are doubled, the quote escaped) takes it back as one argument.
    let plain = !word.is_empty() && !word.chars().any(|c| c.is_whitespace() || "\"&|<>^%()!,;=".contains(c));
    if plain {
        return word.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in word.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

#[cfg(not(windows))]
fn quote(word: &str) -> String {
    let plain = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_reach_the_command_as_they_were_typed() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(shell_join(&words(&["npm test && npm run build"])), "npm test && npm run build", "one argument is a command line");
        assert_eq!(shell_join(&words(&["cargo", "test", "-p", "nitely-core"])), "cargo test -p nitely-core");
        // Through the same shell a lane uses, node gets each word as one argument.
        let shell = |line: &str| -> String {
            #[cfg(windows)]
            let out = {
                use std::os::windows::process::CommandExt;
                std::process::Command::new("cmd").args(["/D", "/S", "/C"]).raw_arg(format!("\"{line}\"")).output().unwrap()
            };
            #[cfg(not(windows))]
            let out = std::process::Command::new("sh").args(["-c", line]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let js = shell_join(&words(&["node", "-e", "console.log('a b')"]));
        assert_eq!(shell(&js), "a b", "{js}");
        let amp = shell_join(&words(&["node", "-e", "console.log(process.argv[1])", "x&y"]));
        assert_eq!(shell(&amp), "x&y", "{amp}");
    }
}
