//! Adding a repo from Nitely: pick a folder, and Nitely creates its project, registers the repo,
//! keeps a playbook for it, and connects every agent installed on this machine (Claude Code, Codex,
//! Cursor) at the user level: the MCP server, the hooks that show live status, and the instructions
//! for working through Nitely. Nothing is written into the repo, so every worktree and branch gets
//! the same setup with no commit. `plan` shows each change before `apply` makes it; running it
//! again changes nothing that is already in place.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::config::{Config, RepoConfig};
use crate::db::Db;
use crate::instructions::{SETUP_DESCRIPTION, SETUP_GUIDE, WORKFLOW};
use crate::playbook::{git, Playbook, PLAYBOOK_PATH};

const BLOCK_START: &str = "<!-- nitely:start -->";
const BLOCK_END: &str = "<!-- nitely:end -->";
const CLAUDE_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    // Before MCP calls: which session makes each one, so the nitely server acts as it (NIT-194).
    // Before shell commands: a build in a light worktree is reminded to use `run` (NIT-202).
    ("PreToolUse", Some("mcp__.*|Bash|PowerShell")),
    ("PostToolUse", Some("*")),
    ("Notification", None),
    ("Stop", None),
    ("SessionEnd", None),
];
const CURSOR_EVENTS: &[&str] = &["beforeSubmitPrompt", "afterFileEdit", "stop"];

/// Matchers an earlier nitely wrote for its own hooks, by event: settings that still have one are
/// brought to the current matcher, as one change.
const OLD_MATCHERS: &[(&str, &str)] = &[("PreToolUse", "mcp__.*")];

#[derive(Debug, Clone, Serialize)]
pub struct ConnectPlan {
    pub root: String,
    pub repo_name: String,
    pub project_key: String,
    pub project_name: String,
    /// The project already exists; the repo joins it.
    pub project_exists: bool,
    pub integration_branch: String,
    pub playbook: PlaybookPlan,
    /// How agents run Nitely: `nitely` when it is on PATH, else its full path.
    pub nitely: String,
    pub agents: Vec<AgentPlan>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaybookPlan {
    /// `repo` (the repo has its own), `existing` (Nitely already keeps one) or `generated`.
    pub source: String,
    pub path: String,
    /// The text Nitely will keep; empty when the repo's own is used.
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentPlan {
    pub agent: String,
    /// Whether the agent seems installed (its config folder exists).
    pub found: bool,
    pub changes: Vec<Change>,
    /// Why its setup could not be read, e.g. a config file that is not valid JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    /// A file path, or the command to run.
    pub target: String,
    /// `create`, `update`, `unchanged`, `run`, or `manual` (the human makes it; `after` says how).
    pub action: String,
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    pub after: String,
}

impl Change {
    fn file(path: &Path, why: &str, after: String) -> Change {
        let before = std::fs::read_to_string(path).ok();
        let action = match &before {
            None => "create",
            Some(b) if *b == after => "unchanged",
            Some(_) => "update",
        };
        Change { target: path.to_string_lossy().into_owned(), action: action.into(), why: why.into(), before, after }
    }

    /// A JSON file, unchanged when it already holds the same value however it is formatted, so a
    /// file the user or another tool reformatted is not reported as out of date.
    fn json(path: &Path, why: &str, value: &Value) -> Change {
        let mut c = Change::file(path, why, pretty(value));
        if c.action == "update" && c.before.as_deref().and_then(|b| serde_json::from_str::<Value>(b).ok()).as_ref() == Some(value) {
            c.action = "unchanged".into();
        }
        c
    }
}

/// The folder agents keep their user-level config (and Claude Code and Codex their sessions) in:
/// the person's home. `NITELY_AGENT_HOME` overrides it, for tests and the test harness, which must
/// never read or write the person's own agents.
pub fn agent_home() -> PathBuf {
    std::env::var_os("NITELY_AGENT_HOME").map(PathBuf::from).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

pub fn plan(db: &Db, config: &Config, path: &str, key: Option<&str>, name: Option<&str>) -> Result<ConnectPlan> {
    let dir = Path::new(path.trim());
    if !dir.is_dir() {
        bail!("{} is not a folder", dir.display());
    }
    let root = crate::git::main_root(&dir.to_string_lossy()).with_context(|| format!("{} is not in a git repo", dir.display()))?;
    let root = root.strip_prefix(r"\\?\").unwrap_or(&root).to_string();
    let root_path = PathBuf::from(&root);
    let existing = config.repos.iter().find(|r| crate::paths::normalize(&r.root) == crate::paths::normalize(&root));
    // A repo Nitely already watches keeps its name, and with it the playbook kept under that name.
    let repo_name = match existing {
        Some(r) => r.name.clone(),
        None => root_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into()),
    };

    let project_name = name.map(str::to_string).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| repo_name.clone());
    let project_key = match key.map(|k| k.trim().to_uppercase()).filter(|k| !k.is_empty()) {
        Some(k) => k,
        None => match existing.and_then(|r| r.project.clone()) {
            Some(k) => k,
            None => suggest_key(db, &project_name)?,
        },
    };
    let project_exists = db.project(&project_key)?.is_some();

    let mut warnings = Vec::new();
    let playbook = playbook_plan(&root_path, &repo_name)?;
    let integration_branch = match playbook.source.as_str() {
        "repo" | "existing" => Playbook::load(&root_path).map(|p| p.integration_branch().to_string()).unwrap_or_default(),
        _ => Playbook::parse(&playbook.text)?.integration_branch().to_string(),
    };
    if git(&root_path, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{integration_branch}")]).is_err() {
        warnings.push(format!("there is no local branch {integration_branch}; edit the playbook's integration branch before starting tasks"));
    }

    let resolved = nitely_command();
    if resolved.is_none() {
        warnings.push(format!("{UNRESOLVED}; new agent entries will run `nitely`, and entries that point at another install are left as they are"));
    }
    let nitely = resolved.clone().unwrap_or_else(|| "nitely".into());
    let agents = with_links(agent_plans(&nitely, resolved.is_some()), config, resolved.as_deref());
    for a in &agents {
        if let Some(e) = &a.error {
            warnings.push(format!("{} is skipped: {e}", a.agent));
        }
    }
    if agents.iter().all(|a| !a.found) {
        warnings.push("no Claude Code, Codex or Cursor configuration was found in your home folder".into());
    }

    Ok(ConnectPlan {
        root,
        repo_name,
        project_key,
        project_name,
        project_exists,
        integration_branch,
        playbook,
        nitely,
        agents,
        warnings,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub project: String,
    pub repo: String,
    pub done: Vec<String>,
    pub failed: Vec<String>,
}

/// Makes every change in the plan, for the agents that were found. A failure for one agent does not
/// stop the others; it is reported.
/// `actor` is whoever connects: the human in the app, an agent through the CLI.
pub fn apply(db: &mut Db, config: &mut Config, plan: &ConnectPlan, actor: &crate::Actor) -> Result<Applied> {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    if !plan.project_exists {
        db.create_project(&plan.project_key, &plan.project_name)?;
        done.push(format!("created project {} ({})", plan.project_key, plan.project_name));
    }
    config.upsert_repo(RepoConfig {
        name: plan.repo_name.clone(),
        root: plan.root.clone(),
        integration_branch: plan.integration_branch.clone(),
        project: Some(plan.project_key.clone()),
    })?;
    config.save()?;
    done.push(format!("watching {} for project {}", plan.root, plan.project_key));

    if plan.playbook.source == "generated" {
        write(Path::new(&plan.playbook.path), &plan.playbook.text)?;
        done.push(format!("wrote its playbook to {}", plan.playbook.path));
    }

    // A project nobody has set up gets its first issue: the sweep that seeds the board and
    // drafts the review rules. One per project, whatever number of repos join it.
    if db.project_brief(&plan.project_key)?.setup.is_none() {
        match file_setup_issue(db, plan, actor) {
            Ok(Some(key)) => done.push(format!("filed {key}: an agent sweeps the repo, seeds the board and drafts the review rules")),
            Ok(None) => {}
            Err(e) => failed.push(format!("filing the setup issue ({e:#})")),
        }
    }

    let (agents_done, agents_failed) = apply_agents(&plan.agents);
    done.extend(agents_done);
    failed.extend(agents_failed);
    Ok(Applied { project: plan.project_key.clone(), repo: plan.repo_name.clone(), done, failed })
}

/// The title of the first issue of a project nobody has set up.
fn setup_title(project: &str) -> String {
    format!("Set up {project}: sweep the repo, seed the board and draft its review rules")
}

/// Files the setup issue, unless an open one is there already. None when it was.
fn file_setup_issue(db: &mut Db, plan: &ConnectPlan, actor: &crate::Actor) -> Result<Option<String>> {
    // The project's own name, which every repo joining it shares; the plan's is the folder's
    // until the project exists.
    let name = db.project(&plan.project_key)?.map(|p| p.name).unwrap_or_else(|| plan.project_name.clone());
    let title = setup_title(&name);
    if db.list_issues(Some(&plan.project_key), false)?.iter().any(|i| i.title == title) {
        return Ok(None);
    }
    let safe = if plan.playbook.source == "generated" {
        "Until rules are approved, every change waits for you (the playbook's approval.always_human); the proposal turns that off along with the rules it drafts."
    } else {
        "The repo already has a playbook; propose only what it is missing."
    };
    let body = format!(
        "Nitely just connected {repo} ({root}) to {key}, and its board is empty. Run the first-run setup sweep: in Claude Code `/nitely-setup`, in any agent the `setup_guide` tool. It reads the repo without changing it, checks the integration branch and the gate first, files what it finds, proposes the review and approval rules (review.focus, approval.human_paths, review.flags, approval.blocked_paths) with propose_playbook_change for the human to approve, and writes the project brief. {safe}\n\nWhen the sweep is done, move this issue to Done with what it filed as evidence.",
        repo = plan.repo_name,
        root = plan.root,
        key = plan.project_key,
    );
    let issue = db.create_issue(
        &crate::NewIssue {
            project: plan.project_key.clone(),
            title,
            body,
            kind: "feature".into(),
            // The agent's vendor when its id names one, as the MCP tools file; the human in the app.
            source: match actor {
                crate::Actor::Human => "human".into(),
                crate::Actor::Agent(id) => match id.split(':').next() {
                    Some(v) if crate::issues::SOURCES.contains(&v) => v.into(),
                    _ => "claude".into(),
                },
            },
            evidence: format!("{} was connected with an empty board and no setup sweep yet.", plan.repo_name),
            priority: 1,
            allow_similar: true,
            ..Default::default()
        },
        actor,
    )?;
    Ok(Some(issue.key))
}

const UNRESOLVED: &str = "the nitely command is neither on PATH nor beside this app (cargo install --path crates/nitely-cli)";

/// One plan per agent. An agent whose setup cannot be read gets its error instead of changes, so
/// one broken config file does not hide what the others need. `repoint` lets entries that run
/// Nitely from another install path be pointed at `nitely`; it is off when `nitely` is only a guess.
fn agent_plans(nitely: &str, repoint: bool) -> Vec<AgentPlan> {
    [("Claude Code", claude_plan(nitely, repoint)), ("Codex", codex_plan(nitely, repoint)), ("Cursor", cursor_plan(nitely, repoint))]
        .into_iter()
        .map(|(agent, plan)| plan.unwrap_or_else(|e| AgentPlan { agent: agent.into(), found: true, changes: Vec::new(), error: Some(format!("{e:#}")) }))
        .collect()
}

/// What it would take to bring every installed agent's setup (MCP server, hooks, skill and
/// instructions) up to this version of Nitely. The MCP server and hooks build their text when they
/// run, so they are always current; the files `connect` wrote are not, and go stale when Nitely
/// changes them. Empty when Nitely watches no repo, so a machine that never connected is not asked.
///
/// When Nitely cannot find its own command it proposes nothing, and each installed agent reports
/// that as its error: rewriting working entries to a command that may not run would break them.
pub fn agent_setup(config: &Config) -> Vec<AgentPlan> {
    if config.repos.is_empty() {
        return Vec::new();
    }
    let nitely = nitely_command();
    with_links(setup_for(nitely.clone()), config, nitely.as_deref())
}

/// The agents' plans and, beside them, the listener for issue links at login (link_service.rs):
/// it is what makes every agent's links open, so it is set up with them, unless the person turned
/// it off or Nitely cannot find its own command.
fn with_links(mut plans: Vec<AgentPlan>, config: &Config, nitely: Option<&str>) -> Vec<AgentPlan> {
    if let Some(change) = nitely.and_then(|n| crate::link_service::setup_change(config, n)) {
        plans.push(AgentPlan { agent: "Issue links".into(), found: true, changes: vec![change], error: None });
    }
    plans
}

/// The full path of the nitely command connect writes (`nitely` on PATH, or a path).
pub fn resolve_nitely(nitely: &str) -> Option<PathBuf> {
    let p = Path::new(nitely);
    if p.components().count() > 1 {
        return Some(p.to_path_buf()).filter(|p| p.exists());
    }
    which(nitely)
}

/// The nitely command agents run, when Nitely can find it (on PATH, or beside this program).
pub fn nitely_command_path() -> Option<PathBuf> {
    nitely_command().and_then(|n| resolve_nitely(&n))
}

fn setup_for(nitely: Option<String>) -> Vec<AgentPlan> {
    match nitely {
        Some(n) => agent_plans(&n, true),
        None => agent_plans("nitely", false)
            .into_iter()
            .map(|a| AgentPlan { changes: Vec::new(), error: a.found.then(|| format!("not checked: {UNRESOLVED}")), ..a })
            .collect(),
    }
}

/// How many changes `agents` still needs, for the agents that are installed.
pub fn pending_changes(agents: &[AgentPlan]) -> usize {
    agents.iter().filter(|a| a.found).flat_map(|a| &a.changes).filter(|c| c.action != "unchanged").count()
}

/// Makes the agents' changes. A failure for one does not stop the others; it is reported, as are
/// agents whose setup could not be read and changes only the human can make.
pub fn apply_agents(agents: &[AgentPlan]) -> (Vec<String>, Vec<String>) {
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for agent in agents.iter().filter(|a| a.found) {
        if let Some(e) = &agent.error {
            failed.push(format!("{}: left as it is ({e})", agent.agent));
        }
        for change in agent.changes.iter().filter(|c| c.action != "unchanged") {
            let result = match change.action.as_str() {
                "run" => run(&change.after),
                "manual" => Err(anyhow::anyhow!("not changed by Nitely; {}", change.after)),
                _ => write(Path::new(&change.target), &change.after),
            };
            match result {
                Ok(()) => done.push(format!("{}: {}", agent.agent, change.why)),
                Err(e) => failed.push(format!("{}: {} ({e:#})", agent.agent, change.why)),
            }
        }
    }
    (done, failed)
}

fn playbook_plan(root: &Path, repo_name: &str) -> Result<PlaybookPlan> {
    if root.join(PLAYBOOK_PATH).exists() {
        return Ok(PlaybookPlan { source: "repo".into(), path: root.join(PLAYBOOK_PATH).to_string_lossy().into_owned(), text: String::new() });
    }
    let stored = Playbook::stored_path(repo_name);
    if stored.exists() {
        return Ok(PlaybookPlan { source: "existing".into(), path: stored.to_string_lossy().into_owned(), text: String::new() });
    }
    Ok(PlaybookPlan { source: "generated".into(), path: stored.to_string_lossy().into_owned(), text: generated_playbook(root) })
}

/// A starting playbook from what the repo has: its default branch and the usual test and build
/// commands for Rust and npm projects.
pub(crate) fn generated_playbook(root: &Path) -> String {
    let branch = git(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .ok()
        .and_then(|b| b.trim().strip_prefix("origin/").map(str::to_string))
        .or_else(|| git(root, &["branch", "--show-current"]).ok().map(|b| b.trim().to_string()))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "main".into());
    let mut setup = Vec::new();
    let mut gate = Vec::new();
    if root.join("Cargo.toml").exists() {
        gate.push("cargo test --workspace".to_string());
    }
    if let Ok(text) = std::fs::read_to_string(root.join("package.json")) {
        if root.join("package-lock.json").exists() {
            setup.push("npm ci".to_string());
        }
        let scripts = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("scripts").cloned()).unwrap_or_default();
        for s in ["lint", "test", "build"] {
            if scripts.get(s).is_some() {
                gate.push(if s == "test" { "npm test".into() } else { format!("npm run {s}") });
            }
        }
    }
    let list = |v: &[String]| v.iter().map(|c| format!("{c:?}")).collect::<Vec<_>>().join(", ");
    format!(
        "# Nitely keeps this playbook for the repo. Edit it here, or commit your own as {PLAYBOOK_PATH}\n\
         # in the repo, which then takes precedence.\n\n\
         [branch]\nintegration = {branch:?}\n\n\
         [worktree]\nsetup = [{}]\n\n\
         [gate]\n# Must pass before a change can land.{}\ncommands = [{}]\n\n\
         [approval]\n# Every change waits for you until this project's review rules are approved. The setup\n\
         # sweep drafts them; approving its proposal puts them in place of this line.\n\
         always_human = true\n",
        list(&setup),
        if gate.is_empty() { " Nothing was detected; add your test and build commands." } else { "" },
        list(&gate)
    )
}

/// The nitely executable as a shell word: quoted when its path has spaces.
fn shell_word(nitely: &str) -> String {
    if nitely.contains(' ') { format!("\"{nitely}\"") } else { nitely.to_string() }
}

fn hook_command(nitely: &str, vendor: &str) -> String {
    format!("{} hook {vendor}", shell_word(nitely))
}

fn is_ours(command: &str, vendor: &str) -> bool {
    command.contains("nitely") && command.contains(&format!("hook {vendor}"))
}

/// Whether `word` is just a nitely executable as `shell_word` writes it: a path to `nitely` or
/// `nitely.exe`, quoted when it has spaces. Anything else (a chain such as `prep&&nitely.exe`, a
/// wrapper) is the user's.
fn is_nitely_exe(word: &str) -> bool {
    let path = match word.strip_prefix('"').and_then(|w| w.strip_suffix('"')) {
        Some(inner) => inner,
        None if word.contains(|c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '<' | '>')) => return false,
        None => word,
    };
    !path.contains('"') && Path::new(path).file_stem().is_some_and(|s| s == "nitely")
}

/// When `hook` is Nitely's, returns true, and (with `repoint`) points it at `command` when it is
/// exactly what `connect` wrote for another install path. A command the user wrapped or chained is
/// left as it is.
fn point_at(hook: &mut Value, vendor: &str, command: &str, repoint: bool) -> bool {
    let Some(current) = hook.get("command").and_then(Value::as_str) else { return false };
    if !is_ours(current, vendor) {
        return false;
    }
    if repoint && current.strip_suffix(&format!(" hook {vendor}")).is_some_and(is_nitely_exe) {
        hook["command"] = json!(command);
    }
    true
}

/// Whether an MCP server entry is exactly what `connect` writes, for any install path: a nitely
/// executable run with `mcp`. A launcher the user put there (a WSL wrapper, an env script) is not.
pub(crate) fn is_nitely_mcp(command: Option<&str>, args: Option<Vec<&str>>) -> bool {
    command.is_some_and(|c| is_nitely_exe(&shell_word(c))) && args == Some(vec!["mcp"])
}

fn claude_plan(nitely: &str, repoint: bool) -> Result<AgentPlan> {
    let dir = agent_home().join(".claude");
    let found = dir.is_dir() || agent_home().join(".claude.json").exists();
    let mut changes = Vec::new();

    let settings_path = dir.join("settings.json");
    let mut settings = read_json(&settings_path)?;
    let command = hook_command(nitely, "claude");
    let hooks = obj(&mut settings, "hooks");
    for (event, matcher) in CLAUDE_EVENTS {
        let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let mut present = false;
        // Nitely's hooks taken out of an entry shared with others, moved whole (a chain or an
        // option the user added comes along) into an entry of their own.
        let mut moved: Vec<Value> = Vec::new();
        for entry in list.as_array_mut().into_iter().flatten() {
            let mut ours = false;
            for h in entry.get_mut("hooks").and_then(Value::as_array_mut).into_iter().flatten() {
                ours |= point_at(h, "claude", &command, repoint);
            }
            let old = entry.get("matcher").and_then(Value::as_str).is_some_and(|m| OLD_MATCHERS.contains(&(event, m)));
            if let (true, true, Some(m)) = (ours, old, matcher) {
                let hooks = entry.get_mut("hooks").and_then(Value::as_array_mut);
                if hooks.as_ref().is_some_and(|h| h.len() == 1) {
                    entry["matcher"] = json!(m);
                } else if let Some(hooks) = hooks {
                    // Other hooks share the entry: they keep its matcher, and Nitely's moves to an
                    // entry of its own, so widening it widens nothing of theirs.
                    let (mine, theirs): (Vec<Value>, Vec<Value>) =
                        hooks.drain(..).partition(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, "claude")));
                    *hooks = theirs;
                    moved.extend(mine);
                }
            }
            present |= ours;
        }
        if let (false, Some(m), Some(a)) = (moved.is_empty(), matcher, list.as_array_mut()) {
            a.push(json!({ "matcher": m, "hooks": moved }));
        }
        if !present {
            let mut entry = json!({ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] });
            if let Some(m) = matcher {
                entry["matcher"] = json!(m);
            }
            if let Some(a) = list.as_array_mut() {
                a.push(entry);
            }
        }
    }
    changes.push(Change::json(
        &settings_path,
        "hooks that show each session's status, give it the repo's instructions at start, tell Nitely which session makes each MCP call, and remind a build in a light worktree to use `run`",
        &settings,
    ));

    let skill = dir.join("skills").join("nitely").join("SKILL.md");
    changes.push(Change::file(
        &skill,
        "a skill with the Nitely workflow",
        format!("---\nname: nitely\ndescription: How to take, report, submit and land work in a repo Nitely tracks. Use it whenever the nitely MCP tools are available or the session says the repo is connected to Nitely.\n---\n\n{WORKFLOW}\n"),
    ));
    let setup = dir.join("skills").join("nitely-setup").join("SKILL.md");
    changes.push(Change::file(
        &setup,
        "a skill for the first-run sweep that seeds a new project's board (/nitely-setup)",
        // Quoted: the description has a colon, which a plain YAML value may not.
        format!("---\nname: nitely-setup\ndescription: {}\n---\n\n{SETUP_GUIDE}", Value::String(SETUP_DESCRIPTION.into())),
    ));

    let claude_json = read_json(&agent_home().join(".claude.json"))?;
    let registered = claude_json.pointer("/mcpServers/nitely");
    let current = registered.is_some_and(|s| s.get("command") == Some(&json!(nitely)) && s.get("args") == Some(&json!(["mcp"])));
    // Another install path is worth fixing; the user's own launcher is not ours to judge.
    let moved = repoint && registered.is_some_and(|s| is_nitely_mcp(s.get("command").and_then(Value::as_str), str_args(s.get("args"))));
    let add = format!("claude mcp add --scope user nitely -- {} mcp", shell_word(nitely));
    let why = "register the nitely MCP server for every project";
    changes.push(match (registered, current || !moved) {
        (None, _) => Change { target: add.clone(), action: "run".into(), why: why.into(), before: None, after: add },
        (Some(_), true) => Change { target: add.clone(), action: "unchanged".into(), why: why.into(), before: None, after: add },
        // Registering again would drop the entry's other settings (env, tokens), and Claude Code
        // rewrites ~/.claude.json itself, so the human makes this one edit.
        (Some(_), false) => Change {
            target: agent_home().join(".claude.json").to_string_lossy().into_owned(),
            action: "manual".into(),
            why: "the nitely MCP server is registered with another command".into(),
            before: None,
            after: format!("in mcpServers.nitely, set command to \"{nitely}\" and args to [\"mcp\"], keeping the rest"),
        },
    });
    Ok(AgentPlan { agent: "Claude Code".into(), found, changes, error: None })
}

fn codex_plan(nitely: &str, repoint: bool) -> Result<AgentPlan> {
    let dir = agent_home().join(".codex");
    let found = dir.is_dir();
    let mut changes = Vec::new();

    let config_path = dir.join("config.toml");
    let text = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", config_path.display()))?;
    let mut server = toml_edit::Table::new();
    server["command"] = toml_edit::value(nitely);
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    server["args"] = toml_edit::value(args);
    if !doc.contains_key("mcp_servers") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["mcp_servers"] = toml_edit::Item::Table(t);
    }
    match doc["mcp_servers"].get_mut("nitely").and_then(|i| i.as_table_like_mut()) {
        None => doc["mcp_servers"]["nitely"] = toml_edit::Item::Table(server),
        // An entry with no command is filled in. One that runs nitely from another install path
        // gets the new command; anything else (the user's launcher) is left alone. Only the command
        // changes, so the rest of the entry and its formatting stay as the user left them.
        Some(t) => {
            let command = t.get("command").and_then(|v| v.as_str()).map(str::to_string);
            let args: Option<Vec<&str>> = t.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect());
            let moved = repoint && command.as_deref() != Some(nitely) && is_nitely_mcp(command.as_deref(), args);
            if command.is_none() {
                t.insert("command", toml_edit::value(nitely));
                let mut a = toml_edit::Array::new();
                a.push("mcp");
                t.insert("args", toml_edit::value(a));
            } else if moved {
                t.insert("command", toml_edit::value(nitely));
            }
        }
    }
    let notify = [nitely, "hook", "codex"];
    let mut notify_warning = None;
    match doc.get("notify").and_then(|n| n.as_array()).map(|a| a.iter().map(|v| v.as_str().unwrap_or("")).collect::<Vec<_>>()) {
        Some(a) if a == notify => {}
        // Not set, or exactly what connect wrote for another install path.
        None => doc["notify"] = toml_edit::value(notify.into_iter().collect::<toml_edit::Array>()),
        Some(a) if a.len() == 3 && is_nitely_exe(&shell_word(a[0])) && a[1..] == ["hook", "codex"] => {
            if repoint {
                doc["notify"] = toml_edit::value(notify.into_iter().collect::<toml_edit::Array>())
            }
        }
        // Nitely inside the user's own wrapper is left alone.
        Some(a) if a.iter().any(|s| s.contains("nitely")) => {}
        Some(_) => notify_warning = Some("Codex already runs another notify program, so Nitely does not see when its turns end".to_string()),
    }
    let mut c = Change::file(&config_path, "the nitely MCP server, and a notify hook that shows when a turn ends", doc.to_string());
    if let Some(w) = notify_warning {
        c.why = format!("{} ({w})", c.why);
    }
    changes.push(c);

    let agents_md = dir.join("AGENTS.md");
    let existing = std::fs::read_to_string(&agents_md).unwrap_or_default();
    let block = format!(
        "{BLOCK_START}\n## Repos connected to Nitely\n\nWhen the `nitely` MCP tools are available, the repo is tracked by Nitely:\n\n{WORKFLOW}\n{BLOCK_END}"
    );
    changes.push(Change::file(&agents_md, "the Nitely workflow in Codex's global instructions", with_block(&existing, &block)));
    Ok(AgentPlan { agent: "Codex".into(), found, changes, error: None })
}

fn cursor_plan(nitely: &str, repoint: bool) -> Result<AgentPlan> {
    let dir = agent_home().join(".cursor");
    let found = dir.is_dir();
    let mut changes = Vec::new();

    let mcp_path = dir.join("mcp.json");
    let mut mcp = read_json(&mcp_path)?;
    let servers = obj(&mut mcp, "mcpServers");
    let server = servers.entry("nitely".to_string()).or_insert_with(|| json!({}));
    if !server.is_object() {
        *server = json!({});
    }
    // As for Codex: fill in a missing command, repoint another install path, leave a launcher.
    let command = server.get("command").and_then(Value::as_str).map(str::to_string);
    if command.is_none() {
        server["command"] = json!(nitely);
        server["args"] = json!(["mcp"]);
    } else if repoint && command.as_deref() != Some(nitely) && is_nitely_mcp(command.as_deref(), str_args(server.get("args"))) {
        server["command"] = json!(nitely);
    }
    changes.push(Change::json(&mcp_path, "the nitely MCP server, with its instructions", &mcp));

    let hooks_path = dir.join("hooks.json");
    let mut hooks_doc = read_json(&hooks_path)?;
    if hooks_doc.get("version").is_none() {
        hooks_doc["version"] = json!(1);
    }
    let command = hook_command(nitely, "cursor");
    let hooks = obj(&mut hooks_doc, "hooks");
    for event in CURSOR_EVENTS {
        let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let mut present = false;
        for h in list.as_array_mut().into_iter().flatten() {
            present |= point_at(h, "cursor", &command, repoint);
        }
        if !present {
            if let Some(a) = list.as_array_mut() {
                a.push(json!({ "command": command }));
            }
        }
    }
    changes.push(Change::json(&hooks_path, "hooks that show each chat's status", &hooks_doc));
    Ok(AgentPlan { agent: "Cursor".into(), found, changes, error: None })
}

/// `text` with the Nitely block replaced, or appended when it has none.
fn with_block(text: &str, block: &str) -> String {
    if let (Some(s), Some(e)) = (text.find(BLOCK_START), text.find(BLOCK_END)) {
        if s < e {
            return format!("{}{block}{}", &text[..s], &text[e + BLOCK_END.len()..]);
        }
    }
    let sep = if text.is_empty() || text.ends_with("\n\n") { "" } else if text.ends_with('\n') { "\n" } else { "\n\n" };
    format!("{text}{sep}{block}\n")
}

/// A JSON array of strings, as `is_nitely_mcp` takes it; None when it is not one.
fn str_args(v: Option<&Value>) -> Option<Vec<&str>> {
    v?.as_array()?.iter().map(Value::as_str).collect()
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(json!({})),
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("{} is not valid JSON; fix it or move it aside first", path.display())),
        Err(_) => Ok(json!({})),
    }
}

fn obj<'a>(v: &'a mut Value, key: &str) -> &'a mut serde_json::Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    let entry = v.as_object_mut().unwrap().entry(key.to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().unwrap()
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default() + "\n"
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write beside and rename, so an agent reading the file never sees half of it.
    let tmp = path.with_extension("nitely-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

fn run(command: &str) -> Result<()> {
    run_in(command, std::env::var_os("NITELY_AGENT_HOME").as_deref())
}

/// Runs an agent's own command. With an agents' home other than the person's (tests, the test
/// harness), the agent's CLI (`claude mcp add --scope user`) finds its config from the home folder,
/// so it gets that one as its home, and none of its own variables pointing back at the person's.
fn run_in(command: &str, agents: Option<&std::ffi::OsStr>) -> Result<()> {
    let mut cmd = crate::playbook::shell_command(command);
    if let Some(home) = agents {
        cmd.env("HOME", home).env("USERPROFILE", home).env_remove("CLAUDE_CONFIG_DIR").env_remove("CODEX_HOME");
    }
    let out = cmd.output().with_context(|| format!("running {command}"))?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed"));
    }
    Ok(())
}

/// A free project key from the name: its capitals (AcmeWeb: AW), else its first letters.
fn suggest_key(db: &Db, name: &str) -> Result<String> {
    let letters: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let caps: String = letters.chars().filter(|c| c.is_ascii_uppercase()).collect();
    let mut base = if caps.len() >= 2 { caps } else { letters.chars().take(3).collect::<String>().to_uppercase() };
    if base.len() < 2 || !base.starts_with(|c: char| c.is_ascii_alphabetic()) {
        base = "PRJ".into();
    }
    base.truncate(6);
    for n in 0..100 {
        let key = if n == 0 { base.clone() } else { format!("{base}{n}") };
        let taken = db.project(&key)?.is_some() || db.issue(&format!("{key}-1"))?.is_some();
        if !taken {
            return Ok(key);
        }
    }
    bail!("no free project key for {name}; choose one")
}

/// `nitely` when it is on PATH; else the full path of the binary next to this one; else None.
fn nitely_command() -> Option<String> {
    if which("nitely").is_some() {
        return Some("nitely".into());
    }
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe)))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd", "bat"] } else { &[""] };
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|e| if e.is_empty() { dir.join(program) } else { dir.join(format!("{program}.{e}")) })
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_block_is_replaced_not_repeated() {
        let block = format!("{BLOCK_START}\nv2\n{BLOCK_END}");
        let once = with_block("# Mine\n", &block);
        assert_eq!(once, format!("# Mine\n\n{block}\n"));
        let old = once.replace("v2", "v1");
        assert_eq!(with_block(&old, &block), once);
        assert_eq!(with_block("", &block), format!("{block}\n"));
    }

    #[test]
    fn an_agents_command_runs_in_the_agents_home_it_was_given() {
        let agents = crate::paths::unique_temp("nitely-agents", "");
        std::fs::create_dir_all(&agents).unwrap();
        let out = agents.join("home.txt");
        let home = if cfg!(windows) { "%USERPROFILE%" } else { "$HOME" };
        run_in(&format!("echo {home}> \"{}\"", out.display()), Some(agents.as_os_str())).unwrap();
        let seen = std::fs::read_to_string(&out).unwrap();
        assert_eq!(seen.trim(), agents.to_string_lossy(), "the command's home is the agents' home, not the person's");
        std::fs::remove_dir_all(&agents).ok();
    }

    #[test]
    fn stale_agent_files_are_found_and_updated() {
        let home = crate::paths::unique_temp("nitely-home", "");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        // Registered already, so nothing runs `claude mcp add`; Codex and Cursor are not installed.
        let claude_json = home.join(".claude.json");
        std::fs::write(&claude_json, r#"{"mcpServers":{"nitely":{"type":"stdio","command":"nitely","args":["mcp"]}}}"#).unwrap();
        std::env::set_var("NITELY_AGENT_HOME", &home);
        let pending_in = |plans: &[AgentPlan]| -> Vec<String> {
            plans.iter().filter(|a| a.found).flat_map(|a| &a.changes).filter(|c| c.action != "unchanged").map(|c| c.target.clone()).collect()
        };

        let first = agent_plans("nitely", true);
        assert_eq!(pending_changes(&first), 3, "the hooks and the two skills are new");
        let setup = home.join(".claude").join("skills").join("nitely-setup").join("SKILL.md");
        assert!(apply_agents(&first).1.is_empty());
        let written = std::fs::read_to_string(&setup).unwrap();
        assert!(written.starts_with("---\nname: nitely-setup\ndescription: \"Set up a project in Nitely: "), "{written}");
        assert_eq!(pending_changes(&agent_plans("nitely", true)), 0);

        // Reformatting a JSON file is not a change; an old copy of the workflow is.
        let settings = home.join(".claude").join("settings.json");
        let compact = serde_json::to_string(&serde_json::from_str::<Value>(&std::fs::read_to_string(&settings).unwrap()).unwrap()).unwrap();
        std::fs::write(&settings, compact).unwrap();
        let skill = home.join(".claude").join("skills").join("nitely").join("SKILL.md");
        std::fs::write(&skill, "an older workflow").unwrap();
        let stale = agent_plans("nitely", true);
        assert_eq!(pending_in(&stale), vec![skill.to_string_lossy().into_owned()]);
        assert!(apply_agents(&stale).1.is_empty());
        assert!(std::fs::read_to_string(&skill).unwrap().contains(WORKFLOW));
        assert_eq!(pending_changes(&agent_plans("nitely", true)), 0);

        // Hooks written by an install at another path are pointed at the current command.
        let text = std::fs::read_to_string(&settings).unwrap();
        std::fs::write(&settings, text.replace("\"nitely hook claude\"", "\"C:/old/nitely.exe hook claude\"")).unwrap();
        let moved = agent_plans("nitely", true);
        assert_eq!(pending_in(&moved), vec![settings.to_string_lossy().into_owned()]);
        assert!(apply_agents(&moved).1.is_empty());
        assert!(!std::fs::read_to_string(&settings).unwrap().contains("C:/old"));

        // Settings from before a hook was added (PreToolUse, NIT-194) are one change: the hook is
        // added beside the others, and the next check finds nothing to do.
        let mut older: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        older["hooks"].as_object_mut().unwrap().remove("PreToolUse");
        std::fs::write(&settings, serde_json::to_string_pretty(&older).unwrap()).unwrap();
        let behind = agent_plans("nitely", true);
        assert_eq!(pending_in(&behind), vec![settings.to_string_lossy().into_owned()]);
        assert_eq!(pending_changes(&behind), 1, "one change to make");
        assert!(apply_agents(&behind).1.is_empty());
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(now["hooks"]["PreToolUse"][0]["matcher"], "mcp__.*|Bash|PowerShell");
        assert_eq!(pending_changes(&agent_plans("nitely", true)), 0);
        // Settings with the MCP-only matcher an earlier nitely wrote are brought up to date, as one
        // change, and no second entry is added.
        let mut narrow: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        narrow["hooks"]["PreToolUse"][0]["matcher"] = json!("mcp__.*");
        std::fs::write(&settings, serde_json::to_string_pretty(&narrow).unwrap()).unwrap();
        let behind = agent_plans("nitely", true);
        assert_eq!(pending_changes(&behind), 1);
        assert!(apply_agents(&behind).1.is_empty());
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!((now["hooks"]["PreToolUse"].as_array().unwrap().len(), &now["hooks"]["PreToolUse"][0]["matcher"]), (1, &json!("mcp__.*|Bash|PowerShell")));
        assert_eq!(pending_changes(&agent_plans("nitely", true)), 0);
        // Another hook in the same old entry keeps its matcher; Nitely's moves out on its own.
        let mut shared: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        shared["hooks"]["PreToolUse"] = json!([{ "matcher": "mcp__.*", "hooks": [
            { "type": "command", "command": "audit-mcp" }, { "type": "command", "command": "nitely hook claude && notify-user", "timeout": 9 }
        ] }]);
        std::fs::write(&settings, serde_json::to_string_pretty(&shared).unwrap()).unwrap();
        assert!(apply_agents(&agent_plans("nitely", true)).1.is_empty());
        let now: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let pre = now["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!((pre.len(), &pre[0]["matcher"], &pre[0]["hooks"][0]["command"], pre[0]["hooks"].as_array().unwrap().len()), (2, &json!("mcp__.*"), &json!("audit-mcp"), 1));
        assert_eq!((&pre[1]["matcher"], &pre[1]["hooks"][0]["command"]), (&json!("mcp__.*|Bash|PowerShell"), &json!("nitely hook claude && notify-user")), "moved as the user wrote it");
        assert_eq!(pre[1]["hooks"][0]["timeout"], 9);
        assert_eq!(pending_changes(&agent_plans("nitely", true)), 0);
        let mut chained = json!({ "command": "nitely hook claude && notify-user" });
        assert!(point_at(&mut chained, "claude", "nitely hook claude", true), "still counts as Nitely's");
        assert_eq!(chained["command"], "nitely hook claude && notify-user", "but is left as the user wrote it");
        let mut prefixed = json!({ "command": "echo ready && C:/tools/nitely.exe hook claude" });
        point_at(&mut prefixed, "claude", "nitely hook claude", true);
        assert_eq!(prefixed["command"], "echo ready && C:/tools/nitely.exe hook claude");
        assert!(!is_nitely_exe("python") && is_nitely_exe("C:/old/nitely.exe") && is_nitely_exe("\"C:/My Tools/nitely.exe\""));
        for chain in ["preflight&&C:/tools/nitely.exe", "a|nitely", "a;nitely", "<in nitely", "x>nitely.exe"] {
            assert!(!is_nitely_exe(chain), "{chain}");
        }
        let mut glued = json!({ "command": "preflight&&C:/tools/nitely.exe hook claude" });
        assert!(point_at(&mut glued, "claude", "nitely hook claude", true));
        assert_eq!(glued["command"], "preflight&&C:/tools/nitely.exe hook claude", "the chain survives");
        let mut spaced = json!({ "command": "\"C:/Program Files/Nitely/nitely.exe\" hook claude" });
        point_at(&mut spaced, "claude", "nitely hook claude", true);
        assert_eq!(spaced["command"], "nitely hook claude");

        // A moved MCP registration is left to the human, so its env and tokens survive.
        std::fs::write(&claude_json, r#"{"mcpServers":{"nitely":{"command":"C:/old/nitely.exe","args":["mcp"],"env":{"T":"x"}}}}"#).unwrap();
        let moved = agent_plans("nitely", true);
        assert_eq!(moved[0].changes.last().unwrap().action, "manual");
        assert_eq!(pending_changes(&moved), 1);
        assert!(apply_agents(&moved).1[0].contains("set command to \"nitely\""));
        assert!(std::fs::read_to_string(&claude_json).unwrap().contains("C:/old"), "left as it was");
        // A launcher the user registered is theirs; only Nitely's own entries move.
        std::fs::write(&claude_json, r#"{"mcpServers":{"nitely":{"command":"wsl","args":["nitely","mcp"]}}}"#).unwrap();
        assert_eq!(agent_plans("nitely", true)[0].changes.last().unwrap().action, "unchanged");

        // Codex and Cursor the same way: another install path is repointed, a launcher is not.
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        let codex = home.join(".codex").join("config.toml");
        let cursor = home.join(".cursor").join("mcp.json");
        std::fs::write(&codex, "[mcp_servers.nitely]
command = \"C:/old/nitely.exe\"
args = [\"mcp\"]
env = { T = \"x\" }
").unwrap();
        std::fs::write(&cursor, r#"{"mcpServers":{"nitely":{"command":"wsl","args":["nitely","mcp"]}}}"#).unwrap();
        let plans = agent_plans("nitely", true);
        let (codex_after, cursor_after) = (&plans[1].changes[0].after, &plans[2].changes[0].after);
        assert!(codex_after.contains("command = \"nitely\"") && codex_after.contains("T = \"x\""), "{codex_after}");
        assert!(cursor_after.contains("\"wsl\"") && !cursor_after.contains("\"command\": \"nitely\""), "{cursor_after}");
        std::fs::write(&codex, "[mcp_servers.nitely]
command = \"wsl\"
args = [\"nitely\", \"mcp\"]
").unwrap();
        std::fs::write(&cursor, r#"{"mcpServers":{"nitely":{"command":"C:/old/nitely.exe","args":["mcp"]}}}"#).unwrap();
        let plans = agent_plans("nitely", true);
        assert!(plans[1].changes[0].after.contains("command = \"wsl\""), "{}", plans[1].changes[0].after);
        assert!(plans[2].changes[0].after.contains("\"command\": \"nitely\""), "{}", plans[2].changes[0].after);

        // When nitely cannot be found, working paths are not repointed to a guess, and the refresh
        // proposes nothing: each installed agent says why instead.
        assert!(agent_plans("nitely", false)[2].changes[0].after.contains("C:/old/nitely.exe"));
        let unresolved = setup_for(None);
        assert!(unresolved.iter().all(|a| a.changes.is_empty()));
        assert!(unresolved.iter().all(|a| a.error.as_deref().is_some_and(|e| e.contains("neither on PATH"))));
        assert_eq!(pending_changes(&unresolved), 0);

        // A config that can't be read is that agent's error, reported as a failure, not a failed check.
        std::fs::write(&settings, "{ not json").unwrap();
        let broken = agent_plans("nitely", true);
        assert!(broken[0].error.as_deref().is_some_and(|e| e.contains("not valid JSON")));
        assert_eq!(broken.len(), 3);
        assert!(apply_agents(&broken).1[0].contains("not valid JSON"));
        std::env::remove_var("NITELY_AGENT_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    /// docs/claude-hooks.json, the hooks for setting Claude Code up by hand (docs/setup.md), names
    /// the same events and matchers `connect` writes.
    #[test]
    fn the_hooks_for_setting_up_by_hand_are_the_ones_connect_writes() {
        let doc: Value = serde_json::from_str(include_str!("../../../docs/claude-hooks.json")).unwrap();
        let hooks = doc["hooks"].as_object().unwrap();
        let written: Vec<(String, Option<String>)> = CLAUDE_EVENTS.iter().map(|(e, m)| (e.to_string(), m.map(str::to_string))).collect();
        let documented: Vec<(String, Option<String>)> = hooks.iter().map(|(e, list)| (e.clone(), list[0]["matcher"].as_str().map(str::to_string))).collect();
        let sorted = |mut v: Vec<(String, Option<String>)>| {
            v.sort();
            v
        };
        assert_eq!(sorted(documented), sorted(written));
        assert!(hooks.values().all(|list| list[0]["hooks"][0]["command"] == "nitely hook claude"));
    }

    #[test]
    fn suggests_free_keys() {
        let path = crate::paths::unique_temp("nitely-connect", ".db");
        let db = Db::open(&path).unwrap();
        assert_eq!(suggest_key(&db, "AcmeWeb").unwrap(), "AW");
        db.create_project("AW", "x").unwrap();
        assert_eq!(suggest_key(&db, "AcmeWeb").unwrap(), "AW1");
        assert_eq!(suggest_key(&db, "nitely").unwrap(), "NIT");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_new_project_gets_one_setup_issue() {
        let dir = crate::paths::unique_temp("nitely-setup-issue", "");
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("SHOP", "Shop").unwrap();
        let plan = ConnectPlan {
            root: "C:/code/shop".into(),
            repo_name: "shop".into(),
            project_key: "SHOP".into(),
            project_name: "Shop".into(),
            project_exists: false,
            integration_branch: "main".into(),
            playbook: PlaybookPlan { source: "generated".into(), path: "shop.toml".into(), text: String::new() },
            nitely: "nitely".into(),
            agents: vec![],
            warnings: vec![],
        };
        let key = file_setup_issue(&mut db, &plan, &crate::Actor::Human).unwrap().unwrap();
        let issue = db.issue(&key).unwrap().unwrap();
        assert_eq!((issue.state.as_str(), issue.owner.as_str(), issue.priority), ("Todo", "agent", 1));
        assert!(issue.body.contains("setup_guide") && issue.body.contains("always_human"), "{}", issue.body);
        assert_eq!(file_setup_issue(&mut db, &plan, &crate::Actor::Human).unwrap(), None, "a second repo joining does not file another");
        // Another repo, named after its own folder, joins the same project: still one issue.
        let other = ConnectPlan { repo_name: "shop-api".into(), project_name: "shop-api".into(), project_exists: true, ..plan.clone() };
        let cli = crate::Actor::Agent("cli".into());
        assert_eq!(file_setup_issue(&mut db, &other, &cli).unwrap(), None);
        // Through the CLI, the issue is an agent's, never the human's.
        db.move_issue(&key, "Canceled", None, Some("redo"), &crate::Actor::Human).unwrap();
        let again = file_setup_issue(&mut db, &other, &cli).unwrap().unwrap();
        let issue = db.issue(&again).unwrap().unwrap();
        assert!(issue.title.contains("Set up Shop:"), "{}", issue.title);
        assert_eq!(db.timeline(&again).unwrap()[0].actor, "cli");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generates_a_playbook_from_the_repo() {
        let dir = crate::paths::unique_temp("nitely-gen", "");
        std::fs::create_dir_all(&dir).unwrap();
        crate::playbook::run_git(std::process::Command::new("git").args(["init", "-q", "-b", "trunk"]).arg(&dir)).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"vitest","build":"vite build"}}"#).unwrap();
        std::fs::write(dir.join("package-lock.json"), "{}").unwrap();
        let pb = Playbook::parse(&generated_playbook(&dir)).unwrap();
        assert_eq!(pb.branch.integration, "trunk");
        assert_eq!(pb.worktree.setup, vec!["npm ci"]);
        assert_eq!(pb.gate.commands, vec!["npm test", "npm run build"]);
        assert!(pb.approval.always_human, "a new repo's changes wait for a human until its rules are approved");
        assert!(pb.unknown.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
