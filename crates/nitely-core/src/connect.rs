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
use crate::instructions::WORKFLOW;
use crate::playbook::{git, Playbook, PLAYBOOK_PATH};

const BLOCK_START: &str = "<!-- nitely:start -->";
const BLOCK_END: &str = "<!-- nitely:end -->";
const CLAUDE_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PostToolUse", Some("*")),
    ("Notification", None),
    ("Stop", None),
    ("SessionEnd", None),
];
const CURSOR_EVENTS: &[&str] = &["beforeSubmitPrompt", "afterFileEdit", "stop"];

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
    /// `repo` (the repo has its own), `existing` (Nitely already keeps one), `aera` or `generated`.
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
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    /// A file path, or the command to run.
    pub target: String,
    /// `create`, `update`, `unchanged` or `run`.
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
}

/// The folder agents keep their user-level config in. `NITELY_AGENT_HOME` overrides it for tests.
fn home() -> PathBuf {
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
    let repo_name = root_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let existing = config.repos.iter().find(|r| crate::paths::normalize(&r.root) == crate::paths::normalize(&root));

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

    let nitely = nitely_command();
    if nitely == "nitely" && which("nitely").is_none() {
        warnings.push("the nitely command is not on PATH; agents need it (cargo install --path crates/nitely-cli)".into());
    }
    let agents = vec![claude_plan(&nitely)?, codex_plan(&nitely)?, cursor_plan(&nitely)?];
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
pub fn apply(db: &Db, config: &mut Config, plan: &ConnectPlan) -> Result<Applied> {
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
    });
    config.save()?;
    done.push(format!("watching {} for project {}", plan.root, plan.project_key));

    if matches!(plan.playbook.source.as_str(), "aera" | "generated") {
        write(Path::new(&plan.playbook.path), &plan.playbook.text)?;
        done.push(format!("wrote its playbook to {}", plan.playbook.path));
    }

    for agent in plan.agents.iter().filter(|a| a.found) {
        for change in agent.changes.iter().filter(|c| c.action != "unchanged") {
            let result = if change.action == "run" { run(&change.after) } else { write(Path::new(&change.target), &change.after) };
            match result {
                Ok(()) => done.push(format!("{}: {}", agent.agent, change.why)),
                Err(e) => failed.push(format!("{}: {} ({e:#})", agent.agent, change.why)),
            }
        }
    }
    Ok(Applied { project: plan.project_key.clone(), repo: plan.repo_name.clone(), done, failed })
}

fn playbook_plan(root: &Path, repo_name: &str) -> Result<PlaybookPlan> {
    if root.join(PLAYBOOK_PATH).exists() {
        return Ok(PlaybookPlan { source: "repo".into(), path: root.join(PLAYBOOK_PATH).to_string_lossy().into_owned(), text: String::new() });
    }
    let stored = Playbook::stored_path(repo_name);
    if stored.exists() {
        return Ok(PlaybookPlan { source: "existing".into(), path: stored.to_string_lossy().into_owned(), text: String::new() });
    }
    let (source, text) = if root.join("scripts/work/work.mjs").exists() && root.join("aera").is_dir() {
        ("aera", include_str!("../../../docs/playbooks/aera.toml").to_string())
    } else {
        ("generated", generated_playbook(root))
    };
    Ok(PlaybookPlan { source: source.into(), path: stored.to_string_lossy().into_owned(), text })
}

/// A starting playbook from what the repo has: its default branch and the usual test and build
/// commands for Rust and npm projects.
fn generated_playbook(root: &Path) -> String {
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
         [gate]\n# Must pass before a change can land.{}\ncommands = [{}]\n",
        list(&setup),
        if gate.is_empty() { " Nothing was detected; add your test and build commands." } else { "" },
        list(&gate)
    )
}

fn hook_command(nitely: &str, vendor: &str) -> String {
    let exe = if nitely.contains(' ') { format!("\"{nitely}\"") } else { nitely.to_string() };
    format!("{exe} hook {vendor}")
}

fn is_ours(command: &str, vendor: &str) -> bool {
    command.contains("nitely") && command.contains(&format!("hook {vendor}"))
}

fn claude_plan(nitely: &str) -> Result<AgentPlan> {
    let dir = home().join(".claude");
    let found = dir.is_dir() || home().join(".claude.json").exists();
    let mut changes = Vec::new();

    let settings_path = dir.join("settings.json");
    let mut settings = read_json(&settings_path)?;
    let command = hook_command(nitely, "claude");
    let hooks = obj(&mut settings, "hooks");
    for (event, matcher) in CLAUDE_EVENTS {
        let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let present = list.as_array().into_iter().flatten().any(|entry| {
            entry.get("hooks").and_then(Value::as_array).into_iter().flatten().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, "claude")))
        });
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
    changes.push(Change::file(&settings_path, "hooks that show each session's status, and give it the repo's instructions at start", pretty(&settings)));

    let skill = dir.join("skills").join("nitely").join("SKILL.md");
    changes.push(Change::file(
        &skill,
        "a skill with the Nitely workflow",
        format!("---\nname: nitely\ndescription: How to take, report, submit and land work in a repo Nitely tracks. Use it whenever the nitely MCP tools are available or the session says the repo is connected to Nitely.\n---\n\n{WORKFLOW}\n"),
    ));

    let registered = read_json(&home().join(".claude.json"))?.pointer("/mcpServers/nitely").is_some();
    let add = format!("claude mcp add --scope user nitely -- {nitely} mcp");
    changes.push(Change {
        target: add.clone(),
        action: if registered { "unchanged" } else { "run" }.into(),
        why: "register the nitely MCP server for every project".into(),
        before: None,
        after: add,
    });
    Ok(AgentPlan { agent: "Claude Code".into(), found, changes })
}

fn codex_plan(nitely: &str) -> Result<AgentPlan> {
    let dir = home().join(".codex");
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
    if doc["mcp_servers"].get("nitely").is_none() {
        doc["mcp_servers"]["nitely"] = toml_edit::Item::Table(server);
    }
    let mut notify_warning = None;
    match doc.get("notify").and_then(|n| n.as_array()) {
        None => {
            let mut a = toml_edit::Array::new();
            a.push(nitely);
            a.push("hook");
            a.push("codex");
            doc["notify"] = toml_edit::value(a);
        }
        Some(a) if a.iter().any(|v| v.as_str() == Some("codex")) && a.iter().any(|v| v.as_str().is_some_and(|s| s.contains("nitely"))) => {}
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
    Ok(AgentPlan { agent: "Codex".into(), found, changes })
}

fn cursor_plan(nitely: &str) -> Result<AgentPlan> {
    let dir = home().join(".cursor");
    let found = dir.is_dir();
    let mut changes = Vec::new();

    let mcp_path = dir.join("mcp.json");
    let mut mcp = read_json(&mcp_path)?;
    let servers = obj(&mut mcp, "mcpServers");
    servers.entry("nitely".to_string()).or_insert_with(|| json!({ "command": nitely, "args": ["mcp"] }));
    changes.push(Change::file(&mcp_path, "the nitely MCP server, with its instructions", pretty(&mcp)));

    let hooks_path = dir.join("hooks.json");
    let mut hooks_doc = read_json(&hooks_path)?;
    if hooks_doc.get("version").is_none() {
        hooks_doc["version"] = json!(1);
    }
    let command = hook_command(nitely, "cursor");
    let hooks = obj(&mut hooks_doc, "hooks");
    for event in CURSOR_EVENTS {
        let list = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let present = list.as_array().into_iter().flatten().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| is_ours(c, "cursor")));
        if !present {
            if let Some(a) = list.as_array_mut() {
                a.push(json!({ "command": command }));
            }
        }
    }
    changes.push(Change::file(&hooks_path, "hooks that show each chat's status", pretty(&hooks_doc)));
    Ok(AgentPlan { agent: "Cursor".into(), found, changes })
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
    let out = crate::playbook::shell_command(command).output().with_context(|| format!("running {command}"))?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed"));
    }
    Ok(())
}

/// A free project key from the name: its capitals (AeraChromium: AC), else its first letters.
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

/// `nitely` when it is on PATH; else the full path of the binary next to this one, if any.
fn nitely_command() -> String {
    if which("nitely").is_some() {
        return "nitely".into();
    }
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe)))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| "nitely".into())
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
    fn suggests_free_keys() {
        let path = std::env::temp_dir().join(format!("nitely-connect-{}-{}.db", std::process::id(), crate::db::now()));
        let db = Db::open(&path).unwrap();
        assert_eq!(suggest_key(&db, "AeraChromium").unwrap(), "AC");
        db.create_project("AC", "x").unwrap();
        assert_eq!(suggest_key(&db, "AeraChromium").unwrap(), "AC1");
        assert_eq!(suggest_key(&db, "nitely").unwrap(), "NIT");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn generates_a_playbook_from_the_repo() {
        let dir = std::env::temp_dir().join(format!("nitely-gen-{}-{}", std::process::id(), crate::db::now()));
        std::fs::create_dir_all(&dir).unwrap();
        std::process::Command::new("git").args(["init", "-q", "-b", "trunk"]).arg(&dir).output().unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"vitest","build":"vite build"}}"#).unwrap();
        std::fs::write(dir.join("package-lock.json"), "{}").unwrap();
        let pb = Playbook::parse(&generated_playbook(&dir)).unwrap();
        assert_eq!(pb.branch.integration, "trunk");
        assert_eq!(pb.worktree.setup, vec!["npm ci"]);
        assert_eq!(pb.gate.commands, vec!["npm test", "npm run build"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
