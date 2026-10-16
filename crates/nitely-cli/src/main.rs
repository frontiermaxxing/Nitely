mod mcp;
mod ui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use nitely_core::issues::branch_name;
use nitely_core::playbook::{self, Playbook};
use nitely_core::{db::NewEvent, hook, paths, resolve_project, Actor, Config, Db, NewIssue, RepoConfig};
use std::io::Read;

/// Nitely: what your coding agents are doing.
#[derive(Parser)]
#[command(name = "nitely", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("NITELY_COMMIT"), ")"))]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record an agent hook event. Reads the hook's JSON on stdin and always exits 0,
    /// so a Nitely problem never blocks the agent.
    Hook {
        /// Which agent sent it: claude, codex or cursor.
        vendor: String,
        /// Codex passes its payload as an argument instead of on stdin.
        payload: Option<String>,
    },
    /// Run the MCP server on stdio (register it as `nitely mcp` in your agent's MCP config).
    Mcp,
    /// Run one of the app's read commands and print its JSON, for the browser preview's live mode.
    /// Only reads: commands that change anything are refused.
    Ui {
        /// The app command, e.g. snapshot, pulse, issue_detail.
        command: String,
        /// Its arguments as a JSON object with the page's names, e.g. '{"key":"NIT-12"}'.
        args: Option<String>,
    },
    /// Run one MCP tool call given on stdin; a running `nitely mcp` forwards calls here once a
    /// newer build is installed.
    #[command(hide = true)]
    McpCall,
    /// Put this build (or --from) where agents run `nitely` from, even while agents run the old
    /// one: on Windows the running one is moved aside, and old copies nothing runs are deleted.
    Install {
        /// The build to install; this executable by default.
        #[arg(long)]
        from: Option<std::path::PathBuf>,
        /// Where to put it; the `nitely` on PATH by default, else ~/.cargo/bin.
        #[arg(long)]
        to: Option<std::path::PathBuf>,
    },
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
    /// Land a submitted issue: rebase, check its review and approval, gate, fast-forward, push.
    Land {
        key: String,
        /// Land locally without pushing.
        #[arg(long)]
        no_push: bool,
        /// Keep the worktree and branch afterwards.
        #[arg(long)]
        keep: bool,
        /// Take over a lock left by a land that died.
        #[arg(long)]
        break_lock: bool,
    },
    /// Bring every installed agent's Nitely setup (MCP server, hooks, skill, instructions) up to
    /// this version of Nitely. Shows what is out of date; changes it only with --yes.
    Agents {
        /// Make the changes.
        #[arg(long)]
        yes: bool,
    },
    /// Add a repo to Nitely: create its project, watch it, keep a playbook for it, and connect
    /// Claude Code, Codex and Cursor (MCP server, hooks, instructions) at the user level.
    /// Shows every change; makes them only with --yes.
    Connect {
        /// Any folder in the repo.
        path: String,
        /// Project key, e.g. AERA; suggested from the name when omitted.
        #[arg(long)]
        project: Option<String>,
        /// Project name; the repo's folder name when omitted.
        #[arg(long)]
        name: Option<String>,
        /// Make the changes.
        #[arg(long)]
        yes: bool,
    },
    /// Manage the repos Nitely watches.
    Repo {
        #[command(subcommand)]
        command: RepoCmd,
    },
    /// Move the Linear project from ~/.aera-work/credentials.json into native issues, once,
    /// keeping its keys (AND-275 stays AND-275), states, priorities, labels, bodies and comments.
    /// Prints what it would do; changes nothing without --yes.
    ImportLinear {
        /// The project the issues go into; its key must be the Linear keys' prefix.
        #[arg(long, default_value = "AND")]
        project: String,
        /// The project's name, if it is created.
        #[arg(long, default_value = "Aera")]
        name: String,
        /// Import, rather than only show what would happen.
        #[arg(long)]
        yes: bool,
        /// Leave out issues archived in Linear (they are imported by default).
        #[arg(long)]
        skip_archived: bool,
    },
    /// Print the Now board as JSON.
    Now,
    /// Print what every agent session did over the last hours (the Activity view) as JSON.
    Activity {
        #[arg(long, default_value_t = 12)]
        hours: i64,
    },
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
        /// An issue that must close before this one can start (repeatable).
        #[arg(long)]
        blocked_by: Vec<String>,
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
    /// Record that an issue cannot start before others close; next_task skips it until then.
    Block {
        key: String,
        /// The blocking issues.
        #[arg(long = "by", required = true)]
        by: Vec<String>,
    },
    /// Remove blockers from an issue.
    Unblock {
        key: String,
        #[arg(long = "by", required = true)]
        by: Vec<String>,
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
        Cmd::Hook { vendor, payload } => {
            if let Err(e) = run_hook(vendor, payload.as_deref()) {
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
        Cmd::McpCall => {
            if let Err(e) = mcp::call_once() {
                eprintln!("nitely mcp-call: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        Cmd::Ui { command, args } => {
            // Refused commands exit 2, so the preview can tell them from failures.
            if !ui::READ_ONLY.contains(&command.as_str()) {
                eprintln!("{}", ui::refusal(command));
                std::process::exit(2);
            }
            let result = serde_json::from_str(args.as_deref().unwrap_or("{}"))
                .map_err(anyhow::Error::from)
                .and_then(|a| ui::run(command, &a));
            match result {
                Ok(v) => println!("{v}"),
                Err(e) => {
                    eprintln!("{e:#}");
                    std::process::exit(1);
                }
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

fn run_hook(vendor: &str, payload: Option<&str>) -> Result<()> {
    let mut input = payload.unwrap_or_default().to_string();
    if input.is_empty() {
        std::io::stdin().read_to_string(&mut input)?;
    }
    let value: serde_json::Value = serde_json::from_str(&input)?;
    let event = match vendor {
        "claude" => hook::from_claude(&value)?,
        "codex" => hook::from_codex(&value)?,
        "cursor" => {
            // Cursor waits for an answer before sending a prompt; always let it through.
            if value.get("hook_event_name").and_then(|v| v.as_str()) == Some("beforeSubmitPrompt") {
                println!("{{\"continue\": true}}");
            }
            hook::from_cursor(&value)?
        }
        other => bail!("unsupported vendor {other}"),
    };
    // Claude Code adds a SessionStart hook's output to the session: the repo's Nitely instructions.
    if vendor == "claude" && event.kind == "session_start" {
        if let Some(text) = nitely_core::instructions::for_dir(&Config::load()?, &event.cwd) {
            println!("{text}");
        }
    }
    Db::open_default()?.record(&event)
}

fn cwd() -> Result<String> {
    Ok(std::env::current_dir()?.to_string_lossy().into_owned())
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn print_agent_changes(agents: &[nitely_core::connect::AgentPlan]) {
    for agent in agents {
        println!("\n{}{}", agent.agent, if agent.found { "" } else { " (not installed; skipped)" });
        if let Some(e) = &agent.error {
            println!("  ERROR     {e}");
        }
        for c in &agent.changes {
            println!("  {:<9} {}\n            {}", c.action, c.target, c.why);
        }
    }
}

fn run(cmd: Cmd) -> Result<()> {
    // Agents run the CLI too, so it never acts as the human: approving and answering asks happen
    // only in the app.
    let me = Actor::Agent("cli".into());
    match cmd {
        Cmd::Hook { .. } | Cmd::Mcp | Cmd::McpCall | Cmd::Ui { .. } => unreachable!(),
        Cmd::Install { from, to } => {
            let from = match from {
                Some(f) => f,
                None => std::env::current_exe()?,
            };
            let to = to.or_else(|| nitely_core::install::default_target(&from)).ok_or_else(|| anyhow::anyhow!("no home folder; pass --to"))?;
            print_json(&nitely_core::install::install(&from, &to)?)
        }
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
                IssueCmd::Create { title, kind, source, evidence, body, priority, project, state, allow_similar, blocked_by } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    let issue = db.create_issue(
                        &NewIssue { project, title, body, kind, source, evidence, priority, allow_similar, state, blocked_by },
                        &me,
                    )?;
                    print_json(&issue)
                }
                IssueCmd::List { project, all } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    for i in db.list_issues(Some(&project), all)? {
                        let blocked = if i.blocked_by.is_empty() { String::new() } else { format!("  (blocked by {})", i.blocked_by.join(", ")) };
                        println!("{:<9} {:<12} {}{blocked}", i.key, i.state, i.title);
                    }
                    Ok(())
                }
                IssueCmd::Show { key } => {
                    let issue = db.issue(&key)?.ok_or_else(|| anyhow::anyhow!("no issue {key}"))?;
                    print_json(&serde_json::json!({
                        "issue": issue,
                        "relations": db.relations(&issue.key)?,
                        "timeline": db.timeline(&issue.key)?,
                        "prs": db.prs_for_issue(&issue.key)?,
                        "reviews": db.reviews(&issue.key)?,
                    }))
                }
                IssueCmd::Move { key, state, evidence, reason } => {
                    print_json(&db.move_issue(&key, &state, evidence.as_deref(), reason.as_deref(), &me)?)
                }
                IssueCmd::Block { key, by } => {
                    db.change_blockers(&key, &by, &[], &me)?;
                    print_json(&db.relations(&key)?)
                }
                IssueCmd::Unblock { key, by } => {
                    db.change_blockers(&key, &[], &by, &me)?;
                    print_json(&db.relations(&key)?)
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
                            blocked_by: Vec::new(),
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
                            println!("nothing ready in {project}");
                            for i in db.list_issues(Some(&project), false)?.iter().filter(|i| i.state == "Todo" && !i.blocked_by.is_empty()) {
                                println!("  {} waits on {}", i.key, i.blocked_by.join(", "));
                            }
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
            for w in &started.warnings {
                eprintln!("warning: {w}");
            }
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
            for w in &gate.warnings {
                eprintln!("warning: {w}");
            }
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
            nitely_core::install::check_current(&config, &cwd, env!("NITELY_COMMIT"))?;
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
        Cmd::Land { key, no_push, keep, break_lock } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            let cwd = cwd()?;
            nitely_core::install::check_current(&config, &cwd, env!("NITELY_COMMIT"))?;
            let opts = nitely_core::land::LandOptions { dir: &cwd, issue_key: &key, push: !no_push, keep, break_lock };
            let r = nitely_core::land::land(&mut db, &config, &opts, &me)?;
            print_json(&r)?;
            if let Some(e) = &r.error {
                eprintln!("land stopped: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Agents { yes } => {
            let config = Config::load()?;
            if config.repos.is_empty() {
                println!("Nitely watches no repo yet; nitely connect <path> sets the agents up");
                return Ok(());
            }
            let agents = nitely_core::connect::agent_setup(&config);
            let pending = nitely_core::connect::pending_changes(&agents);
            print_agent_changes(&agents);
            if pending == 0 {
                if agents.iter().any(|a| a.error.is_some()) {
                    eprintln!("\nfix the errors above, then run again");
                    std::process::exit(1);
                }
                println!("\nevery agent's setup is up to date");
                return Ok(());
            }
            if !yes {
                eprintln!("\n{pending} out of date; run again with --yes to update");
                return Ok(());
            }
            let (done, failed) = nitely_core::connect::apply_agents(&agents);
            println!();
            for d in &done {
                println!("done    {d}");
            }
            for f in &failed {
                println!("FAILED  {f}");
            }
            if !failed.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Connect { path, project, name, yes } => {
            let mut config = Config::load()?;
            let db = Db::open_default()?;
            let plan = nitely_core::connect::plan(&db, &config, &path, project.as_deref(), name.as_deref())?;
            let new = if plan.project_exists { "existing" } else { "new" };
            println!("{} → project {} ({}, {new}), integration branch {}", plan.root, plan.project_key, plan.project_name, plan.integration_branch);
            println!("playbook: {} ({})", plan.playbook.path, plan.playbook.source);
            print_agent_changes(&plan.agents);
            for w in &plan.warnings {
                eprintln!("warning: {w}");
            }
            if !yes {
                eprintln!("\nnothing changed; run again with --yes to apply");
                return Ok(());
            }
            let applied = nitely_core::connect::apply(&db, &mut config, &plan)?;
            println!();
            for d in &applied.done {
                println!("done    {d}");
            }
            for f in &applied.failed {
                println!("FAILED  {f}");
            }
            if !applied.failed.is_empty() {
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
                    config.upsert_repo(RepoConfig { name: name.clone(), root, integration_branch: integration, project })?;
                    config.save()?;
                    println!("watching {name}");
                }
                RepoCmd::List => print_json(&config.repos)?,
            }
            Ok(())
        }
        Cmd::Now => print_json(&nitely_core::snapshot(&Config::load()?, &Db::open_default()?)?),
        Cmd::ImportLinear { project, name, yes, skip_archived } => {
            let creds = nitely_core::credentials::load()?
                .ok_or_else(|| anyhow::anyhow!("no credentials file at {}", nitely_core::credentials::path().display()))?;
            eprintln!("reading every issue of the Linear project, with comments…");
            let issues = nitely_core::linear_import::fetch(&creds, skip_archived)?;
            let mut db = Db::open_default()?;
            let plan = nitely_core::linear_import::plan(&db, &issues, &project, &name)?;
            print_json(&plan)?;
            if !yes {
                eprintln!(
                    "
dry run: would create {} issue(s) ({} to {}) and {} comment(s) in {} {}; run again with --yes to import",
                    plan.create,
                    plan.first_key.as_deref().unwrap_or("-"),
                    plan.last_key.as_deref().unwrap_or("-"),
                    plan.comments,
                    if plan.project_exists { "the existing project" } else { "a new project" },
                    plan.project
                );
                return Ok(());
            }
            print_json(&nitely_core::linear_import::apply(&mut db, &issues, &plan, &me)?)
        }
        Cmd::Activity { hours } => {
            let to = nitely_core::db::now();
            print_json(&nitely_core::activity::activity(&Config::load()?, &Db::open_default()?, to - hours * 3600, to)?)
        }
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
