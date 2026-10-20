//! `nitely run` and `nitely lanes`: build lanes from the command line (docs/build-lanes.md).

use anyhow::{Context, Result};

use nitely_core::lanes;
use nitely_core::playbook::{self, Playbook};
use nitely_core::{resolve_project, Actor, Config, Db};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{cwd, print_json};

/// The repo a project's lanes belong to, and the project's key.
fn repo_of(config: &Config, db: &Db, project: Option<&str>) -> Result<(std::path::PathBuf, String, Playbook)> {
    let dir = cwd()?;
    let project = resolve_project(config, db, project, &dir)?;
    let root = playbook::repo_for_issue(config, Some(&project), &dir)?;
    let pb = Playbook::load_with(&root, config)?;
    Ok((root, project, pb))
}

/// `nitely lanes [status]`: the repo's lanes, its queue and its last runs, as JSON.
pub fn status(project: Option<String>) -> Result<()> {
    let config = Config::load()?;
    let db = Db::open_default()?;
    let (root, project, pb) = repo_of(&config, &db, project.as_deref())?;
    match lanes::status(&db, &root, &pb, &project)? {
        Some(s) => print_json(&s),
        None => {
            eprintln!("{} has no [lanes] in its playbook, so it has no build lanes (docs/build-lanes.md)", root.display());
            Ok(())
        }
    }
}

/// `nitely lanes reset`: every free lane back to a clean checkout of what it holds.
pub fn reset(project: Option<String>) -> Result<()> {
    let config = Config::load()?;
    let db = Db::open_default()?;
    let (root, _, pb) = repo_of(&config, &db, project.as_deref())?;
    for line in lanes::reset(&root, &pb)? {
        println!("{line}");
    }
    Ok(())
}

/// `nitely lanes show <id>`: one run's record, then its log.
pub fn show(id: i64) -> Result<()> {
    let db = Db::open_default()?;
    let run = db.lane_run(id)?.with_context(|| format!("no run {id}"))?;
    print_json(&run)?;
    if let Some(log) = &run.log {
        if let Ok(text) = std::fs::read_to_string(log) {
            println!("\n--- {log}\n{text}");
        }
    }
    Ok(())
}

/// `nitely lanes cancel <run>`: a queued run out of the queue.
pub fn cancel(id: i64, me: &Actor) -> Result<()> {
    let db = Db::open_default()?;
    print_json(&nitely_core::lanes::cancel(&db, id, me)?)
}

/// `nitely run -- <command>`: the current worktree as a snapshot, in a lane, its output followed as
/// it comes. Exits with the command's own exit code.
pub fn run(issue: Option<String>, shell: Option<String>, command: Vec<String>, me: &Actor) -> Result<()> {
    let config = Config::load()?;
    let db = Db::open_default()?;
    let command = match shell {
        Some(line) => line,
        None if command.is_empty() => anyhow::bail!("say what to run: `nitely run -- cargo test`, or `nitely run --shell \"npm test && npm run build\"`"),
        None => shell_join(&command)?,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let mut follower: Option<std::thread::JoinHandle<()>> = None;
    let result = lanes::run_in(&config, &db, &cwd()?, issue.as_deref(), &command, me, &mut |r| match r.state.as_str() {
        "queued" => eprintln!("run {} queued", r.id),
        "running" => {
            if r.executor == "local" { eprintln!("run {} in lane {}", r.id, r.lane.unwrap_or(0)); }
            else { eprintln!("run {} on {}",r.id,r.executor); }
            if let Some(log) = r.log.clone() {
                let stop = stop.clone();
                follower = Some(std::thread::spawn(move || follow(&log, &stop)));
            }
        }
        "lane_error" if r.executor != "local" => eprintln!("run {} on {}: {}",r.id,r.executor,r.error.as_deref().unwrap_or("executor unavailable")),
        _ => {}
    });
    stop.store(true, Ordering::SeqCst);
    if let Some(f) = follower {
        f.join().ok();
    }
    let run = result?;
    if run.executor != "local" && !run.output.is_empty() { println!("{}",run.output); }
    match (&run.error, run.exit_code, run.timed_out) {
        (Some(e), _, _) => {
            eprintln!("run {}: the lane could not run it: {e}", run.id);
            crate::exit(2);
        }
        (None, _, true) => {
            eprintln!("run {}: stopped, past the lane's timeout", run.id);
            crate::exit(124);
        }
        (None, code, _) => {
            let code = code.unwrap_or(1);
            eprintln!("run {}: exit {code}", run.id);
            crate::exit(code);
        }
    }
}

/// The command line the lane's shell runs for `nitely run -- <words>`: each word passed on as the
/// one word it was, quoted where it has spaces, quotes or shell characters, so nothing in it is
/// taken as shell syntax. (`--shell "<line>"` is for a line that means to use the shell.)
fn shell_join(args: &[String]) -> Result<String> {
    Ok(args.iter().map(|a| quote(a)).collect::<Result<Vec<_>>>()?.join(" "))
}

/// A word for cmd. Inside double quotes cmd takes every character but `"` (which ends the quoted
/// part, whatever precedes it) and `%` (which it expands even there) as it is, and the program's
/// argument parser takes the quoted word back as one argument. A word with either cannot be
/// passed through cmd as one word, so it is refused rather than guessed at.
#[cfg(windows)]
fn quote(word: &str) -> Result<String> {
    if word.contains('"') || word.contains('%') {
        anyhow::bail!(
            "the word {word:?} has a \" or % that the lane's shell (cmd) would read as its own; pass the whole line with --shell, quoted for cmd"
        );
    }
    let plain = !word.is_empty() && !word.chars().any(|c| c.is_whitespace() || "&|<>^()!,;=".contains(c));
    if plain {
        return Ok(word.to_string());
    }
    // Backslashes right before the closing quote are doubled, so the parser does not read the
    // last one as escaping it; elsewhere they stay as they are.
    let trailing = word.len() - word.trim_end_matches('\\').len();
    Ok(format!("\"{word}{}\"", "\\".repeat(trailing)))
}

#[cfg(not(windows))]
fn quote(word: &str) -> Result<String> {
    let plain = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    Ok(if plain { word.to_string() } else { format!("'{}'", word.replace('\'', "'\\''")) })
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
        let join = |w: &[&str]| shell_join(&words(w)).unwrap();
        assert_eq!(join(&["cargo", "test", "-p", "nitely-core"]), "cargo test -p nitely-core");
        assert_ne!(join(&["echo", "a&b"]), "echo a&b", "a shell character in a word stays in the word");
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
        let js = join(&["node", "-e", "console.log('a b')"]);
        assert_eq!(shell(&js), "a b", "{js}");
        let echo = |word: &str| join(&["node", "-e", "console.log(process.argv[1])", word]);
        for word in ["x&y", "a | b > c", "C:\\some dir\\", "(1,2);=^!"] {
            assert_eq!(shell(&echo(word)), word, "{}", echo(word));
        }
        // A word cmd cannot carry is refused, never run as something else; sh carries it.
        let tricky = "\"&echo INJECTED&rem \"";
        if cfg!(windows) {
            assert!(shell_join(&words(&["node", "-e", "1", tricky])).unwrap_err().to_string().contains("--shell"));
            assert!(shell_join(&words(&["echo", "%PATH%"])).is_err());
        } else {
            assert_eq!(shell(&echo(tricky)), tricky);
        }
    }
}
