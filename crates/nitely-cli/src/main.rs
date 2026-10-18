// The MCP tool list is one json! literal, deeper than the default macro recursion allows.
#![recursion_limit = "256"]

mod crew;
mod lanes;
mod mcp;
mod newer;
mod serve;
mod ui;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use nitely_core::issues::{branch_name, BatchItem};
use nitely_core::playbook::{self, Playbook};
use nitely_core::{db::NewEvent, hook, paths, resolve_project, Actor, Config, Db, NewIssue, RepoConfig};
use std::io::Read;

/// Nitely: what your coding agents are doing.
#[derive(Parser)]
#[command(name = "nitely", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("NITELY_COMMIT"), ", ", env!("NITELY_BUILD_CHANNEL"), ")"))]
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
    /// Run the MCP tool calls an older `nitely mcp` forwards, one JSON object per line on stdin,
    /// each answered by id when it ends.
    #[command(hide = true)]
    McpCalls,
    /// Run one MCP tool call (a land, a lane run) for a `nitely mcp` that started this process to
    /// outlive it: {vendor, cwd, params, session, out} on stdin, the answer written to `out`.
    #[command(hide = true)]
    McpDetached,
    /// Seed an empty data folder with the large sample board (4 projects, 800 issues, 30
    /// worktrees, 5,000 events) for the app's performance checks (scripts/harness/perf.e2e.mjs).
    /// Only a folder of its own, given as NITELY_HOME, never the person's.
    #[command(hide = true)]
    PerfSeed,
    /// Put this build (or --from) where agents run `nitely` from, even while agents run the old
    /// one: on Windows the running one is moved aside, and old copies nothing runs are deleted.
    Install {
        /// The build to install; this executable by default.
        #[arg(long)]
        from: Option<std::path::PathBuf>,
        /// Where to put it; the `nitely` on PATH by default, else ~/.cargo/bin.
        #[arg(long)]
        to: Option<std::path::PathBuf>,
        /// Install a --from build too old to say whether it opens your database.
        #[arg(long)]
        force: bool,
    },
    /// Exit 0 when this build can open the database, else 3 and say why: what `install --from`
    /// asks another build before putting it in place.
    #[command(hide = true)]
    CheckDatabase,
    /// Report what you are doing, from the current directory's worktree.
    Status {
        /// One line, e.g. "running test:gate".
        text: String,
        /// Session to attach this to; defaults to one per directory.
        #[arg(long)]
        session: Option<String>,
    },
    /// Build lanes and the disk task worktrees use (docs/build-lanes.md): `status` (the default),
    /// `reset`, `show <run>`, `slim`, `leftovers`.
    Lanes {
        #[command(subcommand)]
        command: Option<LanesCmd>,
    },
    /// Issue links (http://127.0.0.1:7423/i/KEY): the small listener that starts at login and opens
    /// them in the app, starting it when it is closed (docs/setup.md, "Issue links").
    Links {
        #[command(subcommand)]
        command: LinksCmd,
    },
    /// Review batches (docs/batching.md): `compare` sets the shadow batches of `[review] batch =
    /// "shadow"` against the single reviews that decided the same changes.
    Batches {
        #[command(subcommand)]
        command: BatchesCmd,
    },
    /// Goals: the outcomes a project works toward, and the issues that get it there.
    Goal {
        #[command(subcommand)]
        command: GoalCmd,
    },
    /// What happened over the last hours, per goal: what landed, what is at risk, when each goal
    /// will be done at its pace, and every place you were needed (as JSON).
    Report {
        /// How far back, in hours (default 12).
        #[arg(long, default_value_t = 12)]
        hours: i64,
        #[arg(long)]
        project: Option<String>,
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
        /// Ask the human to accept this P0/P1 finding of --issue's latest review as it is (its
        /// number from 1, or its text); the question is your reasoning.
        #[arg(long, requires = "issue")]
        finding: Option<String>,
    },
    /// Create an issue's branch and worktree from the integration branch, per the repo's
    /// playbook (.nitely/playbook.toml), and run its setup commands. Prints the worktree.
    Start {
        key: String,
        /// Who the branch is for, e.g. claude or cursor.
        #[arg(long, default_value = "cli")]
        agent: String,
    },
    /// Add paths to the current sparse light worktree (each file's directory, or the directory); a
    /// path in a declared submodule makes or widens that submodule's worktree.
    Widen {
        /// Relative to the repo root, e.g. src/billing src/ui/Meter.tsx.
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Run the playbook's gate commands in the current worktree and record the result.
    Gate {
        /// The issue; defaults to the one the branch names.
        #[arg(long)]
        issue: Option<String>,
    },
    /// Run a build or test command in one of the repo's build lanes (docs/build-lanes.md), on a
    /// snapshot of the current worktree, committed or not, e.g. `nitely run -- cargo test`. Prints
    /// the output as it comes and exits with the command's exit code.
    Run {
        /// The issue; defaults to the one the branch names.
        #[arg(long)]
        issue: Option<String>,
        /// A whole command line for the lane's shell, e.g. "npm test && npm run build"; without
        /// it, the words after -- are passed on as words.
        #[arg(long, conflicts_with = "command")]
        shell: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Hand in the current worktree's branch: gate, cross-vendor review, push, pull request. It runs
    /// in the background and returns a job id (`nitely submit-status`); --wait runs it here.
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
        /// Run the gate and review here and print the result, as submit did before (for scripts).
        #[arg(long)]
        wait: bool,
    },
    /// The result of a submit: by job id, by --issue (its latest), or the current worktree's issue.
    SubmitStatus {
        job: Option<i64>,
        #[arg(long)]
        issue: Option<String>,
        /// Wait up to this many seconds for it to finish.
        #[arg(long)]
        wait: Option<u64>,
    },
    /// Answers a triage question; `triage` starts it as a process of its own.
    #[command(hide = true)]
    TriageJob { id: i64 },
    /// How often the person's Inbox was needed per landed issue, over the last days, and what
    /// triage answered instead (NIT-245).
    InboxRate {
        #[arg(long, default_value = "NIT")]
        project: String,
        #[arg(long, default_value_t = 1)]
        days: u64,
    },
    /// Runs a queued submit; `nitely submit` starts it in the background.
    #[command(hide = true)]
    SubmitJob { id: i64 },
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
    /// Tokens the agents used, read from their own session files (counts only, never text): an
    /// issue's, or a project's per day over the last days.
    Spend {
        /// An issue key; without it, the project's spend.
        issue: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 14)]
        days: usize,
    },
    /// The issue mirror (docs/mirror.md): `status` says what it would publish; `sync` brings the
    /// host up to date now. Only the app turns it on.
    Mirror {
        /// status or sync
        action: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// The project's crew: agent workers Nitely starts and keeps running itself (docs/crew.md).
    Crew {
        #[command(subcommand)]
        command: crew::CrewCmd,
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
        /// Project key, e.g. APP; suggested from the name when omitted.
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
    /// Move a Linear project into native issues, once, keeping its keys (APP-275 stays APP-275),
    /// states, priorities, labels, bodies and comments. Reads the Linear API key from
    /// credentials.json in Nitely's data folder (`nitely where`). Prints what it would do;
    /// changes nothing without --yes.
    ImportLinear {
        /// The project the issues go into; its key must be the Linear keys' prefix (APP for APP-12).
        #[arg(long)]
        project: String,
        /// The project's name, if it is created; the key when omitted.
        #[arg(long)]
        name: Option<String>,
        /// The Linear project's id; the credentials file's `linear.projectId` when omitted.
        #[arg(long)]
        linear_project: Option<String>,
        /// The Nitely state for a Linear state Nitely has no name for, e.g.
        /// --state "Needs Review=Needs You". Repeat for more; it wins over the config's
        /// linear_states. Others map by name, then by type.
        #[arg(long = "state", value_name = "LINEAR=NITELY")]
        states: Vec<String>,
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
    /// Pull PRs from Bitbucket and issues from Linear, using credentials.json in Nitely's data folder, and end the sessions whose agent stopped without saying so.
    Sync,
    /// Print where Nitely keeps its data.
    Where,
    /// Print release notes as Markdown: the issues whose commits landed since a tag, with their
    /// titles and pull requests. The release workflow writes each release's notes with it.
    ReleaseNotes {
        /// The version the notes are for, e.g. 0.2.0.
        #[arg(long)]
        version: String,
        /// The previous release's tag. Without one these are the first release's notes, which list
        /// no history.
        #[arg(long)]
        since: Option<String>,
        /// The last commit in the release.
        #[arg(long, default_value = "HEAD")]
        until: String,
        /// The GitHub repo (owner/name) its pull requests are in, for links.
        #[arg(long)]
        github: Option<String>,
        /// The repo to read; the current folder by default.
        #[arg(long, default_value = ".")]
        repo: String,
    },
    /// Serve this machine's backlog to agents on other machines on the local network: the MCP
    /// server over HTTP at /mcp, answered only for this machine's networks and with a token made
    /// in the app (Other machines), and announced over mDNS so `nitely discover` finds it.
    Serve {
        /// Address and port to listen on.
        #[arg(long, default_value = "0.0.0.0:7421")]
        listen: String,
        /// Do not announce it over mDNS (clients then need its address).
        #[arg(long)]
        no_advertise: bool,
    },
    /// Tokens for `nitely serve`, one per machine that reaches the backlog remotely. They are
    /// made in the app (Other machines), which shows each once; here you can list and remove them.
    Tokens {
        #[command(subcommand)]
        command: TokensCmd,
    },
    /// List Nitely servers announced on the local network, and how to connect an agent to one.
    Discover {
        /// Seconds to listen for announcements.
        #[arg(long, default_value_t = 3)]
        wait: u64,
    },
}

#[derive(Subcommand)]
enum TokensCmd {
    /// List the tokens (names and projects, never the tokens).
    List,
    /// Remove a token; agents using it are refused from then on.
    Remove { name: String },
}

#[derive(Subcommand)]
enum BatchesCmd {
    /// Per change, whether the shadow batch agreed with the single review: verdict, P0/P1 findings
    /// by file and symbol, severities, and seconds and tokens per change.
    Compare {
        /// Every pair as JSON, not the summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LinksCmd {
    /// Whether the listener is registered at login and who answers links now.
    Status,
    /// Register the listener at login (no admin rights: your own login items) and start it now.
    Install,
    /// Take it off at login; the running one stops within a minute and the app answers links again.
    Uninstall,
    /// Run the listener (what login starts): answers links on 127.0.0.1:7423 and opens the app, for
    /// as long as it is registered (`nitely links install`).
    Serve,
}

#[derive(Subcommand)]
enum LanesCmd {
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
    /// Take a queued run out of the queue, when its requester will not come for it.
    Cancel { id: i64 },
    /// Worktrees of closed issues still on disk, most often ones a land had to keep because a
    /// process still used them: each with why it stays. The next land removes the ones nothing
    /// keeps; --remove does it now.
    Leftovers {
        /// Only this project's repos.
        #[arg(long)]
        project: Option<String>,
        /// Remove the ones nothing keeps, with their merged branches.
        #[arg(long)]
        remove: bool,
    },
    /// Delete the build directories (the playbook's [lanes] keep, else target and node_modules) of
    /// task worktrees whose work is finished: nothing uncommitted, the branch pushed or landed, and
    /// nothing using the folder. Lists them and asks first.
    Slim {
        /// Only this project's repos.
        #[arg(long)]
        project: Option<String>,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum GoalCmd {
    /// A project's active goals with their progress (--all for done and dropped ones too).
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// One goal and its issues.
    Show { id: i64 },
    /// Create a goal (no --id: --title and --outcome needed) or change one.
    Save {
        #[arg(long)]
        id: Option<i64>,
        #[arg(long)]
        title: Option<String>,
        /// One paragraph: what is true once it is reached.
        #[arg(long)]
        outcome: Option<String>,
        /// YYYY-MM-DD; an empty value clears it.
        #[arg(long)]
        target: Option<String>,
        /// active, done or dropped.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Put an issue in a goal, or take it out with goal 0.
    Attach { key: String, goal: i64 },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Create a project, e.g. `nitely project create NIT Nitely`.
    Create { key: String, name: String },
    List,
    /// Give a project a new name, e.g. `nitely project rename NIT "Nitely app"`.
    Rename { key: String, name: String },
    /// Give a project a new key: every issue gets the new prefix (NIT-12 becomes NX-12), and the
    /// old key keeps working in links and lookups. Refused while one of its issues is checked out.
    Rekey { key: String, new_key: String },
    /// Remove a project. Refused while it has issues, unless --delete-issues repeats its key,
    /// which deletes them and everything recorded about them.
    Remove {
        key: String,
        #[arg(long, value_name = "KEY")]
        delete_issues: Option<String>,
    },
    /// A project's brief and setup marker; the repo's project when no key is given.
    Show { key: Option<String> },
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
        /// Starting state; Todo by default (Needs You for the human's own work), Backlog for later.
        #[arg(long)]
        state: Option<String>,
        /// `human` for work only the human can do; next_task never hands it out.
        #[arg(long)]
        owner: Option<String>,
        /// An area or platform, e.g. ios or billing (repeatable).
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long)]
        allow_similar: bool,
        /// An issue that must close before this one can start (repeatable).
        #[arg(long)]
        blocked_by: Vec<String>,
        /// An active goal of the project to file it into.
        #[arg(long)]
        goal: Option<i64>,
    },
    /// File up to 10 issues at once from a JSON list (a file, or - for stdin). Each item has a `ref`
    /// and file_issue's fields; its blocked_by may name refs of the same list. Prints each ref's key.
    CreateMany {
        file: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// List open issues (add --all for closed ones too).
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        all: bool,
        /// Only issues with this label.
        #[arg(long)]
        label: Option<String>,
    },
    /// Rewrite an issue's title or body (--body - reads it from stdin), or set its priority. What it
    /// said before stays on its timeline; an issue mirrored from another tracker is changed there.
    Edit {
        key: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        /// 1 urgent, 2 high, 3 medium, 4 low, 0 none; the change goes on the timeline.
        #[arg(long)]
        priority: Option<i64>,
    },
    /// Add or remove labels on an issue.
    Label {
        key: String,
        #[arg(long)]
        add: Vec<String>,
        #[arg(long)]
        remove: Vec<String>,
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
        /// Branch tasks are cut from (e.g. main).
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

/// Runs nitely on a thread with room: clap's generated parser and the dispatch below take deep
/// stack frames in a debug build, past the 1 MB Windows gives a process's main thread, and a new
/// subcommand can tip it over (the binary then dies at start with a stack overflow).
fn main() {
    newer::started();
    let worker = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(cli_main)
        .expect("starting nitely's main thread");
    if worker.join().is_err() {
        // The panic has printed itself.
        std::process::exit(101);
    }
}

fn cli_main() {
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
        Cmd::McpCalls => {
            if let Err(e) = mcp::serve_calls() {
                eprintln!("nitely mcp-calls: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        Cmd::McpDetached => {
            if let Err(e) = mcp::run_detached() {
                eprintln!("nitely mcp-detached: {e:#}");
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
    // Before an MCP call: only which session makes it, for the server it reaches (NIT-194). The
    // PostToolUse hook records the call itself.
    if vendor == "claude" {
        let session = value.get("session_id").and_then(|v| v.as_str()).unwrap_or_default();
        if let Some(call) = hook::claude_mcp_call(&value).filter(|_| !session.is_empty()) {
            return Db::open_default()?.note_tool_call(&call, &format!("claude:{session}"));
        }
        // Before a shell command: a build or test in a light worktree is reminded to use `run`. It
        // still runs; PostToolUse records it as any other.
        if let Some((line, cwd)) = hook::claude_shell(&value) {
            if let Some(n) = nitely_core::light::nudge(&Config::load()?, &cwd, &line) {
                println!("{}", hook::claude_context(&n.message));
                let actor = Actor::Agent(format!("claude:{session}"));
                let at = n.worktree.to_string_lossy();
                Db::open_default()?.log_event(&actor, n.issue.as_deref(), Some(&at), "heavy_local", &format!("Built in its light worktree: {}", n.command))?;
            }
            return Ok(());
        }
    }
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

/// Asks the build at `from` whether it opens the database (`check-database`): (its exit code, what
/// it said), or None when it could not run. It runs as a copy, made executable, by its full path,
/// so a file just written without execute permission runs, and a bare name never finds another
/// nitely on PATH.
fn ask_build(from: &std::path::Path) -> Option<(Option<i32>, String)> {
    let probe = paths::unique_temp("nitely-probe", if cfg!(windows) { ".exe" } else { "" });
    std::fs::copy(from, &probe).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    let out = std::process::Command::new(&probe).arg("check-database").output();
    std::fs::remove_file(&probe).ok();
    let out = out.ok()?;
    Some((out.status.code(), String::from_utf8_lossy(&out.stderr).trim().to_string()))
}

fn run(cmd: Cmd) -> Result<()> {
    // Agents run the CLI too, so it never acts as the human: approving and answering asks happen
    // only in the app.
    let me = Actor::Agent("cli".into());
    match cmd {
        Cmd::Hook { .. } | Cmd::Mcp | Cmd::McpCall | Cmd::McpCalls | Cmd::McpDetached | Cmd::Ui { .. } => unreachable!(),
        Cmd::PerfSeed => {
            let home = paths::data_dir();
            let real = paths::normalize(&paths::real_data_dir().to_string_lossy());
            if std::env::var_os("NITELY_HOME").is_none() || paths::normalize(&home.to_string_lossy()) == real {
                bail!("perf-seed seeds only a data folder of its own: set NITELY_HOME to an empty folder");
            }
            // A new data folder only: one with a config of its own is a profile someone uses.
            if paths::config_path().exists() {
                bail!("{} has a config already; perf-seed seeds only a new data folder", home.display());
            }
            let seeded = nitely_core::perf::seed(&paths::db_path(), &home.join("perf-repos"))?;
            seeded.save()?;
            println!("seeded {}: {} projects, {} issues, {} worktrees, {} events", home.display(), nitely_core::perf::PROJECTS.len(), nitely_core::perf::ISSUES, nitely_core::perf::WORKTREES, nitely_core::perf::EVENTS);
            Ok(())
        }
        Cmd::Serve { listen, no_advertise } => serve::serve(Some(&listen), !no_advertise),
        Cmd::Tokens { command } => match command {
            TokensCmd::List => print_json(&nitely_core::relay::list_tokens()?),
            TokensCmd::Remove { name } => {
                if !nitely_core::relay::remove_token(&name)? {
                    bail!("no token named {name}");
                }
                println!("removed {name}'s token");
                Ok(())
            }
        },
        Cmd::Discover { wait } => {
            let found = serve::discover(std::time::Duration::from_secs(wait))?;
            if found.is_empty() {
                eprintln!("No Nitely server answered on this network within {wait} s. Is `nitely serve` running there, and does its firewall allow it?");
            }
            print_json(&found)
        }
        Cmd::CheckDatabase => match nitely_core::db::refused_by_this_build(&paths::db_path())? {
            Some(why) => {
                eprintln!("{why}");
                std::process::exit(3);
            }
            None => Ok(()),
        },
        Cmd::Install { from, to, force } => {
            let from = match from {
                Some(f) => f,
                None => {
                    // Under `cargo run` (a land's after step), this build's stamp must be its
                    // checkout's commit, or every submit would refuse what it installs (NIT-320).
                    if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
                        let head = nitely_core::playbook::run_git(std::process::Command::new("git").args(["-C", &dir, "rev-parse", "HEAD"])).ok().filter(|o| o.status.success());
                        let head = head.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
                        if let Some(why) = nitely_core::install::stamp_mismatch(env!("NITELY_COMMIT"), &head) {
                            anyhow::bail!(why);
                        }
                    }
                    std::env::current_exe()?
                }
            };
            let to = to.or_else(|| nitely_core::install::default_target(&from)).ok_or_else(|| anyhow::anyhow!("no home folder; pass --to"))?;
            // Tests set NITELY_INSTALL_WITHIN to their own folder: an install that would land
            // anywhere else (the person's ~/.cargo/bin, when the test's PATH finds no copy) is
            // refused, so no test can replace the nitely agents run.
            if let Some(within) = std::env::var_os("NITELY_INSTALL_WITHIN") {
                if !paths::is_within(&to.to_string_lossy(), &std::path::Path::new(&within).to_string_lossy()) {
                    bail!("not installing to {}: NITELY_INSTALL_WITHIN allows only {}", to.display(), std::path::Path::new(&within).display());
                }
            }
            // Never put in place a build that cannot open the person's database: every hook, MCP
            // call and `nitely run` would get its refusal. The app's installer runs this after an
            // update, and a nightly once replaced a newer dev build that way (NIT-239). This build
            // answers for itself; another build is asked (`check-database`), since only it knows
            // its migrations.
            let me = std::env::current_exe()?;
            let itself = from.canonicalize().ok().zip(me.canonicalize().ok()).is_some_and(|(a, b)| a == b);
            let refused = if itself {
                nitely_core::db::refused_by_this_build(&paths::db_path())?
            } else {
                match ask_build(&from) {
                    Some((Some(0), _)) => None,
                    Some((Some(3), why)) => Some(why),
                    _ if force => None,
                    _ => bail!(
                        "not installing {}: it cannot say whether it opens your database (a build older than this check, or not a nitely). Install it with --force to replace {} anyway",
                        from.display(),
                        to.display()
                    ),
                }
            };
            if let Some(why) = refused {
                bail!("not installing {} over {}: it could not open your database ({why}). The nitely there stays", from.display(), to.display());
            }
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
        Cmd::Batches { command: BatchesCmd::Compare { json } } => {
            let db = Db::open_default()?;
            let pairs = nitely_core::review_compare::shadow_pairs(&db)?;
            if json {
                return print_json(&pairs);
            }
            for line in nitely_core::review_compare::Report::of(&pairs).lines() {
                println!("{line}");
            }
            Ok(())
        }
        Cmd::Links { command } => {
            let me_exe = std::env::current_exe()?;
            let s = match command {
                LinksCmd::Serve => return nitely_core::link_service::serve(),
                LinksCmd::Status => nitely_core::link_service::status(&Config::load()?, &me_exe),
                LinksCmd::Install => nitely_core::link_service::install(&me_exe)?,
                LinksCmd::Uninstall => nitely_core::link_service::uninstall(&me_exe)?,
            };
            let state = if !s.allowed {
                "never registered here (test harness, or a stand-in agents' home)".to_string()
            } else if s.off {
                "off (turned off; `nitely links install` turns it on)".to_string()
            } else if s.current {
                format!("on at login: {}", s.place)
            } else if s.registered {
                format!("registered with another nitely at {} (`nitely links install` points it here)", s.place)
            } else {
                "not registered (`nitely links install`)".to_string()
            };
            println!("Issue links at login: {state}");
            println!("Answering on 127.0.0.1:{}: {}", s.port, s.answering.unwrap_or("nothing (links open only while something answers)"));
            Ok(())
        }
        Cmd::Lanes { command: None } => lanes::status(None),
        Cmd::Lanes { command: Some(LanesCmd::Status { project }) } => lanes::status(project),
        Cmd::Lanes { command: Some(LanesCmd::Reset { project }) } => lanes::reset(project),
        Cmd::Lanes { command: Some(LanesCmd::Show { id }) } => lanes::show(id),
        Cmd::Lanes { command: Some(LanesCmd::Cancel { id }) } => lanes::cancel(id, &me),
        Cmd::Lanes { command: Some(LanesCmd::Leftovers { project, remove }) } => {
            let config = Config::load()?;
            let db = Db::open_default()?;
            let mut all = Vec::new();
            for repo in &config.repos {
                if project.as_deref().is_some_and(|p| !repo.project.as_deref().is_some_and(|rp| rp.eq_ignore_ascii_case(p))) {
                    continue;
                }
                let root = std::path::Path::new(&repo.root);
                let pb = playbook::Playbook::load(root).unwrap_or_default();
                all.extend(nitely_core::leftovers::leftovers(&db, root, &pb, remove, &me)?);
            }
            if all.is_empty() {
                println!("No landed issue's worktree is left.");
            }
            for l in &all {
                match (&l.stays, &l.removed) {
                    (Some(why), _) => println!("  stays  {}  {}: {why}", l.issue, l.worktree.display()),
                    (None, Some(done)) => println!("  gone   {}  {}: {done}", l.issue, l.worktree.display()),
                    (None, None) => println!("  goes   {}  {}", l.issue, l.worktree.display()),
                }
            }
            if !remove && all.iter().any(|l| l.stays.is_none()) {
                println!("The next land removes the ones that go; --remove removes them now.");
            }
            Ok(())
        }
        Cmd::Lanes { command: Some(LanesCmd::Slim { project, yes }) } => {
            use nitely_core::slim::{human, plan, reclaim};
            let config = Config::load()?;
            let db = Db::open_default().ok();
            let plan = plan(&config, db.as_ref(), project.as_deref())?;
            if plan.is_empty() {
                println!("No task worktree holds build output.");
                return Ok(());
            }
            println!("Build output in task worktrees (an agent's recent session, unfinished work or a running process keeps it):");
            for c in &plan {
                let name = c.worktree.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let dirs: Vec<&str> = c.dirs.iter().map(|(d, _)| d.as_str()).collect();
                match &c.skip {
                    None => println!("  {:>8}  {name}  ({})", human(c.bytes), dirs.join(", ")),
                    Some(why) => println!("  {:>8}  {name}  kept: {why}", human(c.bytes)),
                }
            }
            let go: Vec<_> = plan.iter().filter(|c| c.skip.is_none()).collect();
            if go.is_empty() {
                println!("Nothing to reclaim: every worktree with build output still holds work or is in use.");
                return Ok(());
            }
            let total: u64 = go.iter().map(|c| c.bytes).sum();
            if !yes {
                use std::io::{IsTerminal, Write};
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!("pass --yes to delete {} of build output in {} worktrees without asking", human(total), go.len());
                }
                print!("Delete {} of build output in {} worktrees? Their source and branches stay. [y/N] ", human(total), go.len());
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                    println!("Nothing deleted.");
                    return Ok(());
                }
            }
            // Looked at again: an agent may have started work, or a build, while you decided.
            let asked: Vec<&std::path::Path> = go.iter().map(|c| c.worktree.as_path()).collect();
            let now: Vec<_> = nitely_core::slim::plan(&config, db.as_ref(), project.as_deref())?
                .into_iter()
                .filter(|c| asked.contains(&c.worktree.as_path()))
                .collect();
            for c in now.iter().filter(|c| c.skip.is_some()) {
                println!("  kept after all: {} ({})", c.worktree.display(), c.skip.as_deref().unwrap_or(""));
            }
            let r = reclaim(&now);
            println!("Reclaimed {} from {} worktrees.", human(r.bytes), r.worktrees);
            for f in &r.failed {
                eprintln!("could not remove {f}");
            }
            Ok(())
        }
        Cmd::Report { hours, project } => {
            let db = Db::open_default()?;
            let since = nitely_core::db::now() - hours.max(1) * 3600;
            print_json(&db.goal_report(project.as_deref(), since)?)
        }
        Cmd::Goal { command } => {
            let db = Db::open_default()?;
            match command {
                GoalCmd::List { project, all } => {
                    let project = resolve_project(&Config::load()?, &db, project.as_deref(), &cwd()?)?;
                    print_json(&db.goals(&project, all)?)
                }
                GoalCmd::Show { id } => print_json(&db.goal_detail(id)?),
                GoalCmd::Save { id, title, outcome, target, state, project } => {
                    let project = match id {
                        Some(_) => String::new(),
                        None => resolve_project(&Config::load()?, &db, project.as_deref(), &cwd()?)?,
                    };
                    let change = nitely_core::goals::GoalChange { title, outcome, target_date: target, state, ..Default::default() };
                    print_json(&db.save_goal(&project, id, &change, &me)?)
                }
                GoalCmd::Attach { key, goal } => print_json(&db.set_issue_goal(&key, Some(goal).filter(|g| *g > 0), &me)?),
            }
        }
        Cmd::Project { command } => {
            let mut db = Db::open_default()?;
            match command {
                ProjectCmd::Create { key, name } => print_json(&db.create_project(&key, &name)?),
                ProjectCmd::List => print_json(&db.projects()?),
                ProjectCmd::Rename { key, name } => print_json(&db.rename_project(&key, &name)?),
                ProjectCmd::Rekey { key, new_key } => print_json(&nitely_core::projects::rekey(&mut db, &key, &new_key)?),
                ProjectCmd::Remove { key, delete_issues } => {
                    let with_issues = match delete_issues {
                        Some(k) if k.eq_ignore_ascii_case(&key) => true,
                        Some(k) => anyhow::bail!("--delete-issues must repeat the project's key ({key}), not {k}"),
                        None => false,
                    };
                    print_json(&nitely_core::projects::remove(&mut db, &key, with_issues)?)
                }
                ProjectCmd::Show { key } => {
                    let key = resolve_project(&Config::load()?, &db, key.as_deref(), &cwd()?)?;
                    print_json(&db.project_brief(&key)?)
                }
            }
        }
        Cmd::Issue { command } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            match command {
                IssueCmd::Create { title, kind, source, evidence, body, priority, project, state, owner, labels, allow_similar, blocked_by, goal } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    let issue = db.create_issue(
                        &NewIssue { project, title, body, kind, source, evidence, priority, allow_similar, state, owner, blocked_by, labels, goal },
                        &me,
                    )?;
                    print_json(&issue)
                }
                IssueCmd::CreateMany { file, project } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    let text = if file == "-" { std::io::read_to_string(std::io::stdin())? } else { std::fs::read_to_string(&file)? };
                    let items: Vec<serde_json::Value> = serde_json::from_str(&text).context("the file must hold a JSON list of issues")?;
                    let field = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let opt = |v: &serde_json::Value, k: &str| Some(field(v, k)).filter(|s| !s.is_empty());
                    let strings = |v: &serde_json::Value, k: &str| -> Vec<String> {
                        v.get(k).and_then(|b| b.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect()).unwrap_or_default()
                    };
                    let batch: Vec<BatchItem> = items
                        .iter()
                        .map(|item| BatchItem {
                            not_duplicates: vec![],
                            local_ref: field(item, "ref"),
                            issue: NewIssue {
                                project: project.clone(),
                                title: field(item, "title"),
                                kind: field(item, "kind"),
                                body: field(item, "body"),
                                evidence: field(item, "evidence"),
                                source: opt(item, "source").unwrap_or_else(|| "human".into()),
                                priority: item.get("priority").and_then(|p| p.as_i64()).unwrap_or(0),
                                allow_similar: item.get("allow_similar").and_then(|b| b.as_bool()).unwrap_or(false),
                                state: opt(item, "state"),
                                owner: opt(item, "owner"),
                                blocked_by: strings(item, "blocked_by"),
                                labels: strings(item, "labels"),
                                goal: item.get("goal").and_then(|g| g.as_i64()).filter(|g| *g > 0),
                            },
                        })
                        .collect();
                    nitely_core::issues::check_batch_size(batch.len())?;
                    print_json(&db.create_issues(&batch, &me)?)
                }
                IssueCmd::Edit { key, title, body, priority } => {
                    if title.is_none() && body.is_none() && priority.is_none() {
                        bail!("give --title, --body, --priority or more than one");
                    }
                    let body = match body.as_deref() {
                        Some("-") => {
                            let mut text = String::new();
                            std::io::stdin().read_to_string(&mut text)?;
                            Some(text)
                        }
                        _ => body,
                    };
                    // Before anything is written: a bad priority leaves the title and body as they were.
                    if let Some(p) = priority {
                        nitely_core::issues::check_priority(p)?;
                    }
                    let key = db.current_key(&key)?;
                    let mut issue = db.edit_issue(&key, title.as_deref(), body.as_deref(), &me)?;
                    if let Some(priority) = priority {
                        issue = db.set_priority(&key, priority, &me)?;
                    }
                    print_json(&issue)
                }
                IssueCmd::Label { key, add, remove } => {
                    let key = db.current_key(&key)?;
                    print_json(&db.change_labels(&key, &add, &remove, &me)?)
                }
                IssueCmd::List { project, all, label } => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    let label = label.as_deref().and_then(nitely_core::issues::normalize_label);
                    for i in db.list_issues(Some(&project), all)?.into_iter().filter(|i| label.as_ref().is_none_or(|l| i.labels.contains(l))) {
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
                // A key from before its project changed key names the same issue.
                IssueCmd::Move { key, state, evidence, reason } => {
                    let key = db.current_key(&key)?;
                    db.check_open_asks(&key, &state)?;
                    print_json(&db.move_issue(&key, &state, evidence.as_deref(), reason.as_deref(), &me)?)
                }
                IssueCmd::Block { key, by } => {
                    let key = db.current_key(&key)?;
                    db.change_blockers(&key, &by, &[], &me)?;
                    print_json(&db.relations(&key)?)
                }
                IssueCmd::Unblock { key, by } => {
                    let key = db.current_key(&key)?;
                    db.change_blockers(&key, &[], &by, &me)?;
                    print_json(&db.relations(&key)?)
                }
                IssueCmd::Note { key, text, handoff } => {
                    let key = db.current_key(&key)?;
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
                            owner: Some(field(&item, "owner")).filter(|s| !s.is_empty()),
                            allow_similar: false,
                            blocked_by: Vec::new(),
                            goal: None,
                            labels: item
                                .get("labels")
                                .and_then(|a| a.as_array())
                                .map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect())
                                .unwrap_or_default(),
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
                    match nitely_core::overlap::next_task(&config, &mut db, &project, &agent, &[])? {
                        Some(nitely_core::overlap::Picked { issue: i, why }) => {
                            let branch = match playbook::repo_root(&config, &cwd) {
                                Some(root) => Playbook::load(&root)?.branch_for(&agent, &i),
                                None => branch_name(&agent, &i.key, &i.title),
                            };
                            print_json(&serde_json::json!({ "issue": i, "branch": branch, "why": why }))
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
        Cmd::Ask { question, options, issue, wait, finding } => {
            let mut db = Db::open_default()?;
            let ask = match (&finding, &issue) {
                (Some(finding), Some(issue)) => db.request_acceptance(issue, finding, &question, Some(&cwd()?), &me)?,
                _ => db.create_ask(&question, &options, issue.as_deref(), Some(&cwd()?), &me)?,
            };
            if !wait {
                return print_json(&ask);
            }
            eprintln!("waiting for an answer to ask {} in the Nitely app…", ask.id);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let now = db.ask(ask.id)?.with_context(|| format!("ask {} vanished", ask.id))?;
                if let Some(answer) = now.answer {
                    println!("{answer}");
                    return Ok(());
                }
                // Its issue closed first: no answer is coming.
                if let Some(why) = now.closed {
                    bail!("ask {} {why}, unanswered", ask.id);
                }
            }
        }
        Cmd::Start { key, agent } => {
            let config = Config::load()?;
            let db = Db::open_default()?;
            let cwd = cwd()?;
            let issue = db.issue(&key)?.ok_or_else(|| anyhow::anyhow!("no issue {key}"))?;
            let root = playbook::repo_for_issue(&config, issue.project.as_deref(), &cwd)?;
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
        Cmd::Widen { paths } => {
            let (worktree, _) = playbook::current_worktree(&cwd()?)?;
            let widened = nitely_core::light::widen(&worktree, &paths)?;
            println!("{}", widened.note);
            if !widened.sparse.is_empty() {
                println!("Now holds: {}", widened.sparse.join(", "));
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
        Cmd::Submit { issue, no_review, local, wait } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            let cwd = cwd()?;
            nitely_core::install::check_current(&config, &cwd, env!("NITELY_COMMIT"))?;
            let opts = nitely_core::submit::SubmitOptions { dir: &cwd, issue_key: issue.as_deref(), skip_review: no_review, local_only: local, batch: false };
            if !wait {
                let runner = nitely_core::submit_job::DetachedProcess { nitely: std::env::current_exe()? };
                let job = nitely_core::submit_job::submit_async(&mut db, &config, &opts, &me, &runner)?;
                print_json(&job)?;
                if job.finished() {
                    eprintln!("{}", job.outcome());
                } else {
                    eprintln!("handed in as job {}: the gate and review run in the background. `nitely submit-status {}` shows the result (--wait to wait for it).", job.id, job.id);
                }
                return Ok(());
            }
            eprintln!("running the gate, then the review (this takes minutes)…");
            let s = nitely_core::submit::submit(&mut db, &config, &opts, &me)?;
            print_json(&s)?;
            eprintln!("{}", s.next);
            if !s.gate.passed || s.review.as_ref().is_some_and(|r| r.verdict != "pass") {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::TriageJob { id } => {
            let config = Config::load()?;
            let mut db = match Db::open_default() {
                Ok(db) => db,
                Err(e) if newer::is_newer_database(&e) => {
                    newer::hand_over_or_return();
                    bail!("{}", newer::older_build(&e));
                }
                Err(e) => return Err(e),
            };
            print_json(&nitely_core::triage::answer(&mut db, &config, id)?)
        }
        Cmd::InboxRate { project, days } => {
            let db = Db::open_default()?;
            let since = nitely_core::db::now() - days as i64 * 86_400;
            print_json(&nitely_core::triage::inbox_rate(&db, &project, since)?)
        }
        Cmd::SubmitJob { id } => {
            let config = Config::load()?;
            let mut db = match Db::open_default() {
                Ok(db) => db,
                // Older than the database: the job goes to the newer build being installed, or
                // says why it stopped, not that its process ended for no reason (NIT-314).
                Err(e) if newer::is_newer_database(&e) => {
                    newer::hand_over_or_return();
                    nitely_core::submit_job::fail_unopened(&paths::db_path(), id, &newer::older_build(&e))?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            let job = nitely_core::submit_job::run_in_background(&mut db, &config, id)?;
            print_json(&job)?;
            Ok(())
        }
        Cmd::SubmitStatus { job, issue, wait } => {
            let db = Db::open_default()?;
            let found = match (job, issue.as_deref()) {
                (Some(id), _) => db.submit_job(id)?.ok_or_else(|| anyhow::anyhow!("no submit job {id}"))?,
                (None, Some(key)) => db.latest_submit_job(key)?.ok_or_else(|| anyhow::anyhow!("{key} has no submit yet"))?,
                (None, None) => {
                    let (_, branch_key) = playbook::current_worktree(&cwd()?)?;
                    let key = branch_key.ok_or_else(|| anyhow::anyhow!("pass a job id or --issue"))?;
                    db.latest_submit_job(&key)?.ok_or_else(|| anyhow::anyhow!("{key} has no submit yet"))?
                }
            };
            let found = nitely_core::submit_job::wait(&db, found.id, std::time::Duration::from_secs(wait.unwrap_or(0)))?;
            print_json(&found)?;
            eprintln!("{}", found.outcome());
            Ok(())
        }
        Cmd::Land { key, no_push, keep, break_lock } => {
            let config = Config::load()?;
            let mut db = Db::open_default()?;
            let cwd = cwd()?;
            let issue = db.issue(&key)?.ok_or_else(|| anyhow::anyhow!("no issue {key}"))?;
            let root = playbook::repo_for_issue(&config, issue.project.as_deref(), &cwd)?;
            nitely_core::install::check_current(&config, &root.to_string_lossy(), env!("NITELY_COMMIT"))?;
            let opts = nitely_core::land::LandOptions { dir: &cwd, issue_key: &key, push: !no_push, keep, break_lock };
            let r = nitely_core::land::land(&mut db, &config, &opts, &me)?;
            print_json(&r)?;
            if let Some(e) = &r.error {
                eprintln!("land stopped: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Spend { issue, project, days } => {
            let config = Config::load()?;
            let db = Db::open_default()?;
            match issue {
                Some(key) => print_json(&nitely_core::usage::spend_of_issue(&config, &db, &db.current_key(&key)?)?),
                None => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    // Days in UTC here; the app passes local midnight.
                    let today = nitely_core::db::now().div_euclid(86_400) * 86_400;
                    print_json(&nitely_core::usage::spend_of_project(&config, &db, &project, today, days)?)
                }
            }
        }
        Cmd::Mirror { action, project } => {
            let config = Config::load()?;
            let db = Db::open_default()?;
            match action.as_str() {
                "status" => {
                    let project = resolve_project(&config, &db, project.as_deref(), &cwd()?)?;
                    print_json(&nitely_core::mirror::preview(&config, &db, &project, true)?)
                }
                "sync" => print_json(&nitely_core::mirror::sync(&config, &db)),
                other => bail!("nitely mirror takes status or sync, not {other}"),
            }
        }
        Cmd::Crew { command } => crew::run(command, &me),
        Cmd::Run { issue, shell, command } => lanes::run(issue, shell, command, &me),
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
            let mut db = Db::open_default()?;
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
            let applied = nitely_core::connect::apply(&mut db, &mut config, &plan, &me)?;
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
        Cmd::ImportLinear { project, name, linear_project, states, yes, skip_archived } => {
            let overrides = nitely_core::linear_import::overrides(&Config::load()?.linear_states, &states)?;
            let creds = nitely_core::credentials::load()?
                .ok_or_else(|| anyhow::anyhow!("no credentials file at {}", nitely_core::credentials::path().display()))?;
            eprintln!("reading every issue of the Linear project, with comments…");
            let issues = nitely_core::linear_import::fetch(&creds, linear_project.as_deref(), skip_archived)?;
            let mut db = Db::open_default()?;
            let name = name.unwrap_or_else(|| project.trim().to_uppercase());
            let plan = nitely_core::linear_import::plan(&db, &issues, &project, &name, &overrides)?;
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
            let mut db = Db::open_default()?;
            let report = nitely_core::remote::sync(&Config::load()?, &mut db);
            // As the app's sync does: sessions whose agent stopped without saying so end (NIT-189).
            if let Err(e) = nitely_core::sessions::end_gone_now(&db) {
                eprintln!("ending gone sessions: {e:#}");
            }
            print_json(&report)?;
            if !report.credentials_found {
                eprintln!("no credentials file at {}", nitely_core::credentials::path().display());
            }
            if !report.errors.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::ReleaseNotes { version, since, until, github, repo } => {
            // Titles come from this machine's issues when it has them; CI has none and uses the
            // commits' own subjects. Read-only: notes never change the board.
            let Some(since) = since else {
                print!("{}", nitely_core::release_notes::first_release(&version));
                return Ok(());
            };
            let db = paths::db_path().exists().then(|| Db::open_read_only(&paths::db_path()).ok()).flatten();
            let (entries, other) = nitely_core::release_notes::entries(std::path::Path::new(&repo), Some(&since), &until, db.as_ref())?;
            print!("{}", nitely_core::release_notes::markdown(&version, &entries, &other, github.as_deref()));
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
