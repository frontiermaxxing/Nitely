//! `nitely crew`: run, inspect, pause and resume a project's crew of agent workers (docs/crew.md).

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use nitely_core::crew::{self, Moment, Supervisor, SystemAgents};
use nitely_core::playbook::{self, Playbook};
use nitely_core::{paths, resolve_project, Actor, Config, Db};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Seconds between the supervisor's passes.
const TICK_SECS: u64 = 10;

#[derive(Subcommand)]
pub enum CrewCmd {
    /// Keep the playbook's [crew] workers running on the project's backlog, in the foreground.
    /// Ctrl+C once stops starting workers and waits for the running ones; again stops every
    /// worker that is not landing.
    Run {
        /// The project; the current repo's by default.
        #[arg(long)]
        project: Option<String>,
        /// The repo whose playbook the crew follows, when the project has several.
        #[arg(long)]
        repo: Option<String>,
        /// One pass, then exit (running workers keep running).
        #[arg(long)]
        once: bool,
    },
    /// Print the crew's state as JSON: running workers, cap, today's count, paused.
    Status {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Start no new workers; running ones finish their issue.
    Pause {
        #[arg(long)]
        project: Option<String>,
    },
    /// Start workers again after a pause.
    Resume {
        #[arg(long)]
        project: Option<String>,
    },
}

pub fn run(cmd: CrewCmd, me: &Actor) -> Result<()> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    match cmd {
        CrewCmd::Pause { project } => {
            let project = resolve_project(&config, &db, project.as_deref(), &cwd)?;
            db.crew_pause(&project, true, me)?;
            println!("paused the {project} crew: running workers finish, no new ones start");
            Ok(())
        }
        CrewCmd::Resume { project } => {
            let project = resolve_project(&config, &db, project.as_deref(), &cwd)?;
            db.crew_pause(&project, false, me)?;
            println!("resumed the {project} crew");
            Ok(())
        }
        CrewCmd::Status { project, repo } => {
            let (project, root) = crew_repo(&config, &db, project.as_deref(), repo.as_deref(), &cwd)?;
            let status = crew::status(&db, &project, &Playbook::load(&root)?, &Moment::now())?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        CrewCmd::Run { project, repo, once } => {
            let (project, root) = crew_repo(&config, &db, project.as_deref(), repo.as_deref(), &cwd)?;
            supervise(&mut db, &project, root, once)
        }
    }
}

/// The project and the repo whose playbook its crew follows.
fn crew_repo(config: &Config, db: &Db, project: Option<&str>, repo: Option<&str>, cwd: &str) -> Result<(String, PathBuf)> {
    let project = resolve_project(config, db, project, cwd)?;
    if let Some(repo) = repo {
        let root = playbook::repo_root(config, repo).with_context(|| format!("{repo} is not inside a git repo"))?;
        return Ok((project, root));
    }
    if let Some(here) = config.repo_for_dir(cwd).filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&project))) {
        return Ok((project, PathBuf::from(&here.root)));
    }
    let linked: Vec<_> = config.repos.iter().filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&project))).collect();
    match linked.as_slice() {
        [one] => Ok((project, PathBuf::from(&one.root))),
        [] => bail!("no repo is linked to {project}; link one with `nitely repo add <path> --project {project}`"),
        _ => bail!("{project} has several repos; pass --repo with the one whose playbook the crew follows"),
    }
}

fn supervise(db: &mut Db, project: &str, root: PathBuf, once: bool) -> Result<()> {
    let presses = Arc::new(AtomicUsize::new(0));
    {
        let presses = presses.clone();
        ctrlc::set_handler(move || {
            presses.fetch_add(1, Ordering::SeqCst);
        })
        .context("listening for Ctrl+C")?;
    }
    db.crew_claim_supervisor(project, std::process::id())?;
    // The heartbeat stays fresh through a long pass, so a second `crew run` never takes over.
    let done = Arc::new(AtomicBool::new(false));
    let beat = {
        let (done, project) = (done.clone(), project.to_string());
        std::thread::spawn(move || {
            let Ok(db) = Db::open_default() else { return };
            while !done.load(Ordering::SeqCst) {
                db.crew_heartbeat_touch(&project, std::process::id()).ok();
                for _ in 0..60 {
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        })
    };
    let interrupted = presses.clone();
    let mut sup = Supervisor::new(project, &root, &paths::data_dir().join("crew"), &nitely_command(), Box::new(SystemAgents))
        .with_interrupt(move || interrupted.load(Ordering::SeqCst) > 0);
    eprintln!("crew for {project}, following {}", root.join(playbook::PLAYBOOK_PATH).display());
    eprintln!("Ctrl+C once: start no more workers and wait for the running ones. Twice: stop every worker that is not landing.");

    let mut last_status = String::new();
    let mut seen = 0;
    loop {
        let pressed = presses.load(Ordering::SeqCst);
        if pressed != seen {
            seen = pressed;
            match pressed {
                1 => eprintln!("stopping: no new workers; waiting for {} running (Ctrl+C again to stop them)", sup.running()),
                _ => {
                    let landing = sup.stop_all(db)?;
                    if !landing.is_empty() {
                        eprintln!("stopped the others; waiting for {} to finish landing", landing.join(", "));
                    }
                }
            }
        }
        match sup.tick(db, pressed >= 1) {
            Ok(tick) => {
                for w in &tick.ended {
                    eprintln!("{} ({}) ended: {}. Log: {}", w.issue, w.agent, w.outcome, w.log);
                }
                for w in &tick.started {
                    eprintln!("{} ({}) started. Log: {}", w.issue, w.agent, w.log);
                }
                let line = format!("{} running of {}: {}", tick.running, tick.cap, tick.status);
                if line != last_status {
                    eprintln!("{line}");
                    last_status = line;
                }
            }
            // A busy database or a git hiccup: say it and try again next pass.
            Err(e) => eprintln!("crew pass failed: {e:#}"),
        }
        if once || (pressed >= 1 && sup.running() == 0) {
            break;
        }
        for _ in 0..TICK_SECS * 4 {
            if presses.load(Ordering::SeqCst) != seen {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    done.store(true, Ordering::SeqCst);
    beat.join().ok();
    sup.leave(db)?;
    if !once {
        eprintln!("crew for {project} stopped");
    }
    Ok(())
}

/// How workers start the nitely MCP server: `nitely` when it is on PATH (so `nitely install`
/// updates what they run), else this executable.
fn nitely_command() -> String {
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| dir.join(if cfg!(windows) { "nitely.exe" } else { "nitely" }).is_file())
    });
    if on_path {
        return "nitely".into();
    }
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "nitely".into())
}
