mod mcp;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use nitely_core::issues::branch_name;
use nitely_core::playbook::{self, Playbook};
use nitely_core::{db::NewEvent, hook, paths, resolve_project, Actor, Config, Db, NewIssue, RepoConfig};
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
        /// Which agent sent it. Only `claude` is supported so far.
        vendor: String,
    },
    /// Run the MCP server on stdio (register it as `nitely mcp` in your agent's MCP config).
    Mcp,
    /// Report what you are doing, from the current directory's worktree.
    Status {
        /// One line, e.g. "running test:gate".
        text: String,
        /// Session to attach this to; defaults to one per directory.
        #[arg(long)]
        session: Option<String>,
    },
    /// Projects: a backlog with its own issue prefix.
    Project {
        #[command(subcommand)]
        command: ProjectCmd,
    },
    /// Issues in a project's backlog.
    Issue {
        #[command(subcommand)]
        command: IssueCmd,
    },
    /// Ask the human a question; it shows in the app until answered.
    Ask {
        question: String,
        /// A possible answer; repeat for several.
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long)]
        issue: Option<String>,
        /// Wait for the answer and print it.
        #[arg(long)]
        wait: bool,
    },
    /// Create an issue's branch and worktree from the integration branch, per the repo's
    /// playbook (.nitely/playbook.toml), and run its setup commands. Prints the worktree.
    Start {
        key: String,
        /// Who the branch is for, e.g. claude or cursor.
        #[arg(long, default_value = "cli")]
        agent: String,
    },
    /// Run the playbook's gate commands in the current worktree and record the result.
    Gate {
        /// The issue; defaults to the one the branch names.
        #[arg(long)]
        issue: Option<String>,
    },
    /// Hand in the current worktree's branch: gate, cross-vendor review, push, pull request.
    Submit {
        /// The issue; defaults to the one the branch names.
        #[arg(long)]
        issue: Option<String>,
        /// Skip the review; the change then waits for a human's approval.
        #[arg(long)]
        no_review: bool,
        /// Do not push or open a pull request.
        #[arg(long)]
        local: bool,
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
enum ProjectCmd {
    /// Create a project, e.g. `nitely project create NIT Nitely`.
    Create { key: String, name: String },
    List,
}

#[derive(Subcommand)]
enum IssueCmd {
    /// File an issue. Fails when an open issue has a similar title, unless --allow-similar.
    Create {
        #[arg(long)]
        title: String,
        /// bug, feature, improvement, idea or live-check.
        #[arg(long)]
        kind: String,
        /// Who found it: claude, codex, cursor or human.
        #[arg(long)]
        source: String,
        /// What was seen or run; "none yet" for an idea.
        #[arg(long)]
        evidence: String,
        /// What is wrong or wanted, and why.
        #[arg(long)]
        body: String,
        /// 1 urgent, 2 high, 3 medium, 4 low; 0 or omitted for none.
        #[arg(long, default_value_t = 0)]
        priority: i64,
        #[arg(long)]
        project: Option<String>,
        /// Starting state; Todo by default, Backlog for later.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        allow_similar: bool,
    },
    /// List open issues (add --all for closed ones too).
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Show an issue with its timeline, pull requests and reviews.
    Show { key: String },
    /// Move an issue. Done needs --evidence; Canceled and Duplicate need --reason.
    Move {
        key: String,
        state: String,
        #[arg(long)]
        evidence: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Add a note to an issue's timeline (--handoff for a handoff note).
    Note {
        key: String,
        text: String,
        #[arg(long)]
        handoff: bool,
    },
    /// File every issue in a JSON array of {title, kind, body, evidence?, priority?, state?, source?},
    /// skipping any that look like an open issue's duplicate.
    Import {
        file: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Claim the most urgent Todo issue and print it.
    Next {
        #[arg(long)]
        project: Option<String>,
        /// Who is claiming, e.g. claude or codex.
        #[arg(long, default_value = "cli")]
        agent: String,
    },
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
        /// The project this repo's work belongs to (its issue prefix).
        #[arg(long)]
        project: Option<String>,
    },
    List,
}

fn main() {
    let cli = Cli::parse();
    match &cli.command {
        Cmd::Hook { vendor } => {
            if let Err(e) = run_hook(vendor) {
                eprintln!("nitely hook: {e:#}");
            }
            return;
        }
        Cmd::Mcp => {
            if let Err(e) = mcp::serve() {
                eprintln!("nitely mcp: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        _ => {}
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

fn cwd() -> Result<String> {
    Ok(std::env::current_dir()?.to_string_lossy().into_owned())
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn run(cmd: Cmd) -> Result<()> {
    // Agents run the CLI too, so it never acts as the human: approving and answering asks happen
    // only in the app.
    let me = Actor::Agent("cli".into());
    match cmd {
        Cmd::Hook { .. } | Cmd::Mcp => unreachable!(),
        Cmd::Status { text, session } => {
            let cwd = cwd()?;
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
        Cmd::Project { command } => {
            let db = Db::open_default()?;
            match command {
                ProjectCmd::Create { key, name } => print_json(&db.create_project(&key, &name)?),
                ProjectCmd::List => print_json(&db.projects()?),
            }
        }
        Cmd::Issue { command } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            match command {
                IssueCmd::Create { title, kind, source, evidence, body, priority, project, state, allow_similar } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    let issue = db.create_issue(
                        &NewIssue { project, title, body, kind, source, evidence, priority, allow_similar, state },
                        &me,
                    )?;
                    print_json(&issue)
                }
                IssueCmd::List { project, all } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    for i in db.list_issues(Some(&project), all)? {
                        println!("{:<9} {:<12} {}", i.key, i.state, i.title);
                    }
                    Ok(())
                }
                IssueCmd::Show { key } => {
                    let issue = db.issue(&key)?.ok_or_else(|| anyhow::anyhow!("no issue {key}"))?;
                    print_json(&serde_json::json!({
                        "issue": issue,
                        "timeline": db.timeline(&issue.key)?,
                        "prs": db.prs_for_issue(&issue.key)?,
                        "reviews": db.reviews(&issue.key)?,
                    }))
                }
                IssueCmd::Move { key, state, evidence, reason } => {
                    print_json(&db.move_issue(&key, &state, evidence.as_deref(), reason.as_deref(), &me)?)
                }
                IssueCmd::Note { key, text, handoff } => {
                    db.add_note(&key, &text, if handoff { "handoff" } else { "note" }, &me)
                }
                IssueCmd::Import { file, project } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    let text = std::fs::read_to_string(&file)?;
                    let items: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                    let field = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    for item in items {
                        let new = NewIssue {
                            project: project.clone(),
                            title: field(&item, "title"),
                            kind: field(&item, "kind"),
                            body: field(&item, "body"),
                            evidence: Some(field(&item, "evidence")).filter(|e| !e.is_empty()).unwrap_or_else(|| "none yet".into()),
                            source: Some(field(&item, "source")).filter(|s| !s.is_empty()).unwrap_or_else(|| "human".into()),
                            priority: item.get("priority").and_then(|p| p.as_i64()).unwrap_or(0),
                            state: Some(field(&item, "state")).filter(|s| !s.is_empty()),
                            allow_similar: false,
                        };
                        match db.create_issue(&new, &me) {
                            Ok(i) => println!("{:<9} {}", i.key, i.title),
                            Err(e) => println!("skipped   {}: {}", new.title, e.to_string().lines().next().unwrap_or("")),
                        }
                    }
                    Ok(())
                }
                IssueCmd::Next { project, agent } => {
                    let cwd = cwd()?;
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd)?;
                    match db.next_task(&project, &agent)? {
                        Some(i) => {
                            let branch = match playbook::repo_root(&config, &cwd) {
                                Some(root) => Playbook::load(&root)?.branch_for(&agent, &i),
                                None => branch_name(&agent, &i.key, &i.title),
                            };
                            print_json(&serde_json::json!({ "issue": i, "branch": branch }))
                        }
                        None => {
                            println!("nothing in Todo for {project}");
                            Ok(())
                        }
                    }
                }
            }
        }
        Cmd::Ask { question, options, issue, wait } => {
            let db = Db::open_default()?;
            let ask = db.create_ask(&question, &options, issue.as_deref(), Some(&cwd()?), &me)?;
            if !wait {
                return print_json(&ask);
            }
            eprintln!("waiting for an answer to ask {} in the Nitely app…", ask.id);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                if let Some(answer) = db.ask(ask.id)?.and_then(|a| a.answer) {
                    println!("{answer}");
                    return Ok(());
                }
            }
        }
        Cmd::Start { key, agent } => {
            let config = Config::load()?;
            let db = Db::open_default()?;
            let cwd = cwd()?;
            let issue = db.issue(&key)?.ok_or_else(|| anyhow::anyhow!("no issue {key}"))?;
            let root = playbook::repo_root(&config, &cwd).ok_or_else(|| anyhow::anyhow!("{cwd} is not inside a git repo"))?;
            let started = playbook::start_task(&db, &root, &issue, &agent, &me)?;
            for r in &started.setup {
                eprintln!("{} {} ({}s)", if r.ok { "ok  " } else { "FAIL" }, r.command, r.seconds);
                if !r.ok {
                    eprintln!("{}", r.output);
                }
            }
            println!("{}", started.worktree);
            if started.setup.iter().any(|r| !r.ok) {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Gate { issue } => {
            let config = Config::load()?;
            let db = Db::open_default()?;
            let cwd = cwd()?;
            let (worktree, branch_key) = playbook::current_worktree(&cwd)?;
            let root = playbook::repo_root(&config, &cwd).ok_or_else(|| anyhow::anyhow!("{cwd} is not inside a git repo"))?;
            let key = issue.or(branch_key);
            let gate = playbook::run_gate(&db, &root, &worktree, key.as_deref(), &me)?;
            for r in &gate.results {
                eprintln!("{} {} ({}s)", if r.ok { "ok  " } else { "FAIL" }, r.command, r.seconds);
                if !r.ok {
                    eprintln!("{}", r.output);
                }
            }
            println!("gate {} at {}", if gate.passed { "passed" } else { "failed" }, gate.commit);
            if !gate.passed {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Submit { issue, no_review, local } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            let cwd = cwd()?;
            eprintln!("running the gate, then the review (this takes minutes)…");
            let opts = nitely_core::submit::SubmitOptions { dir: &cwd, issue_key: issue.as_deref(), skip_review: no_review, local_only: local };
            let s = nitely_core::submit::submit(&mut db, &config, &opts, &me)?;
            print_json(&s)?;
            eprintln!("{}", s.next);
            if !s.gate.passed || s.review.as_ref().is_some_and(|r| r.verdict != "pass") {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Repo { command } => {
            let mut config = Config::load()?;
            match command {
                RepoCmd::Add { root, integration, name, project } => {
                    let root = std::fs::canonicalize(&root)
                        .map(|p| clean_path(&p.to_string_lossy()))
                        .unwrap_or(root);
                    let name = name.unwrap_or_else(|| {
                        root.replace('\\', "/").rsplit('/').next().unwrap_or("repo").to_string()
                    });
                    let project = match project {
                        Some(p) => Some(
                            Db::open_default()?
                                .project(&p)?
                                .ok_or_else(|| anyhow::anyhow!("no project {p}"))?
                                .key,
                        ),
                        None => None,
                    };
                    config.upsert_repo(RepoConfig { name: name.clone(), root, integration_branch: integration, project });
                    config.save()?;
                    println!("watching {name}");
                }
                RepoCmd::List => print_json(&config.repos)?,
            }
            Ok(())
        }
        Cmd::Now => print_json(&nitely_core::snapshot(&Config::load()?, &Db::open_default()?)?),
        Cmd::Sync => {
            let report = nitely_core::remote::sync(&Config::load()?, &mut Db::open_default()?);
            print_json(&report)?;
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
