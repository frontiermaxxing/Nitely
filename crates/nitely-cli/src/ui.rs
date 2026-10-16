//! `nitely ui <command> [json-args]`: what the app's window reads, for the browser preview's live
//! mode (vite.config.ts serves it to the page). Same command names and arguments as the Tauri
//! commands in src-tauri/src/lib.rs, same core calls, JSON on stdout.
//!
//! It only reads. Commands are allowed by name; anything that writes (moving or approving an
//! issue, answering an ask, adding a project or a repo, changing agent setup, syncing) is refused
//! here, whatever the page asks, so a browser tab can never act for the human.

use anyhow::{anyhow, bail, Context, Result};
use nitely_core::{Config, Db};
use serde_json::{json, Value};

/// The commands the live preview may run. Every one only reads.
pub const READ_ONLY: &[&str] = &[
    "snapshot",
    "projects",
    "project_brief",
    "crew_status",
    "issues",
    "issue_detail",
    "pulse",
    "activity",
    "agent_setup",
    "opened_link",
    "network",
];

/// Why a command is refused, for the page to show.
pub fn refusal(command: &str) -> String {
    format!(
        "The live preview is read-only: `{command}` is not one of its reads, and changes are made only in the app. Its reads: {}.",
        READ_ONLY.join(", ")
    )
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name).and_then(Value::as_str).with_context(|| format!("missing {name}"))
}

fn int_arg(args: &Value, name: &str) -> Result<i64> {
    args.get(name).and_then(Value::as_i64).with_context(|| format!("missing {name}"))
}

/// Runs a read-only command. `args` uses the page's names (camelCase), as Tauri's invoke does.
pub fn run(command: &str, args: &Value) -> Result<Value> {
    if !READ_ONLY.contains(&command) {
        bail!(refusal(command));
    }
    let db = || Db::open_read_only(&nitely_core::paths::db_path());
    Ok(match command {
        "snapshot" => json!(nitely_core::snapshot(&Config::load_read_only()?, &db()?)?),
        "projects" => json!(db()?.projects()?),
        "project_brief" => json!(db()?.project_brief(str_arg(args, "project")?)?),
        "crew_status" => {
            let moment = nitely_core::crew::Moment::now();
            json!(nitely_core::crew::project_status(&db()?, &Config::load_read_only()?, str_arg(args, "project")?, &moment)?)
        }
        "issues" => {
            let closed = args.get("includeClosed").and_then(Value::as_bool).unwrap_or(false);
            json!(db()?.list_issues(Some(str_arg(args, "project")?), closed)?)
        }
        "issue_detail" => {
            let key = str_arg(args, "key")?;
            json!(db()?.issue_detail(key)?.ok_or_else(|| anyhow!("no issue {key}"))?)
        }
        "pulse" => json!(nitely_core::pulse(&db()?, int_arg(args, "since")?, int_arg(args, "dayStart")?)?),
        "activity" => {
            let to = nitely_core::db::now();
            let hours = int_arg(args, "hours")?.clamp(1, 24 * 7);
            json!(nitely_core::activity::activity(&Config::load_read_only()?, &db()?, to - hours * 3600, to)?)
        }
        "agent_setup" => agents_without_files(&nitely_core::connect::agent_setup(&Config::load_read_only()?)),
        // Machines allowed through `nitely serve`: names and projects, never a token.
        "network" => json!({
            "tokens": nitely_core::relay::list_tokens()?,
            "urls": nitely_core::relay::lan_urls(),
            "port": nitely_core::relay::serving_port().unwrap_or(nitely_core::relay::DEFAULT_PORT),
        }),
        // Links open the app's own window, never the preview.
        "opened_link" => Value::Null,
        _ => unreachable!("every allowed command is handled"),
    })
}

/// Agent setup is read from whole config files (~/.claude.json, Codex's config.toml, ...), which
/// can hold tokens in MCP headers or environment settings, and its file contents and read errors
/// can quote them. Credentials are never printed, so the preview gets only fields that cannot hold
/// them: each agent's name and whether it is installed, each change's target, action and reason,
/// and whether its setup could be read at all. Built field by field, so a new field stays out.
fn agents_without_files(agents: &[nitely_core::connect::AgentPlan]) -> Value {
    json!(agents
        .iter()
        .map(|a| json!({
            "agent": a.agent,
            "found": a.found,
            "error": a.error.as_ref().map(|_| UNREADABLE),
            "changes": a.changes.iter().map(|c| json!({
                "target": c.target,
                "action": c.action,
                "why": c.why,
                "after": HIDDEN,
            })).collect::<Vec<_>>(),
        }))
        .collect::<Vec<_>>())
}

const HIDDEN: &str = "(not shown in the live preview: config files can hold credentials)";
const UNREADABLE: &str = "Nitely could not read this agent's setup; open the app to see why.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_refused_before_anything_runs() {
        for command in ["move_issue", "answer_ask", "set_blocker", "create_project", "connect_plan", "connect_apply", "agent_setup_apply", "sync_now", "approve", ""] {
            let e = run(command, &json!({ "key": "NIT-1", "state": "Approved", "id": 1, "answer": "yes" })).unwrap_err();
            assert!(e.to_string().contains("read-only"), "{command}: {e}");
        }
    }

    #[test]
    fn config_files_and_their_errors_never_reach_the_page() {
        use nitely_core::connect::{AgentPlan, Change};
        let plan = vec![AgentPlan {
            agent: "codex".into(),
            found: true,
            error: Some("config.toml line 3: token = \"secret\"".into()),
            changes: vec![Change {
                target: "~/.codex/config.toml".into(),
                action: "update".into(),
                why: "MCP server".into(),
                before: Some("token = \"secret\"".into()),
                after: "token = \"secret\"".into(),
            }],
        }];
        let out = agents_without_files(&plan).to_string();
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("config.toml") && out.contains("update") && out.contains(HIDDEN) && out.contains(UNREADABLE));
    }

    #[test]
    fn the_database_is_opened_read_only() {
        // Process ids come back on Windows, so the time keeps an earlier run's file out of the way.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("nitely-ui-{}-{nanos}", std::process::id()));
        let path = dir.join("t.db");
        assert!(Db::open_read_only(&path).is_err(), "no database is created");
        drop(Db::open(&path).unwrap());
        let db = Db::open_read_only(&path).unwrap();
        assert!(db.create_project("NIT", "Nitely").is_err(), "writes fail");
        assert!(db.projects().unwrap().is_empty());
    }

    #[test]
    fn the_allowed_commands_are_the_reads() {
        for command in READ_ONLY {
            for verb in ["apply", "move", "answer", "create", "set_", "sync", "approve"] {
                assert!(!command.contains(verb), "{command}");
            }
        }
    }
}
