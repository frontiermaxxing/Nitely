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
    "issues",
    "issue_detail",
    "pulse",
    "activity",
    "agent_setup",
    "connect_plan",
    "opened_link",
];

/// Why a command is refused, for the page to show.
pub fn refusal(command: &str) -> String {
    format!(
        "The live preview is read-only: `{command}` changes Nitely, so only the app can do it. Read-only commands: {}.",
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
    let db = Db::open_default;
    Ok(match command {
        "snapshot" => json!(nitely_core::snapshot(&Config::load()?, &db()?)?),
        "projects" => json!(db()?.projects()?),
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
            json!(nitely_core::activity::activity(&Config::load()?, &db()?, to - hours * 3600, to)?)
        }
        "agent_setup" => without_files(json!(nitely_core::connect::agent_setup(&Config::load()?))),
        "connect_plan" => {
            let opt = |n: &str| args.get(n).and_then(Value::as_str);
            without_files(json!(nitely_core::connect::plan(&db()?, &Config::load()?, str_arg(args, "path")?, opt("key"), opt("name"))?))
        }
        // Links open the app's own window, never the preview.
        "opened_link" => Value::Null,
        _ => unreachable!("every allowed command is handled"),
    })
}

/// What a planned agent config change would write, and what is there now, are whole config files
/// (~/.claude.json, Codex's config.toml, ...), which can hold tokens in MCP headers or environment
/// settings. Credentials are never printed, so the preview gets each change without the files:
/// its target, action and reason, enough for the setup banner and the list of changes.
fn without_files(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(map) => {
                if map.contains_key("target") && map.contains_key("action") {
                    map.remove("before");
                    if map.contains_key("after") {
                        map.insert("after".into(), Value::String(HIDDEN.into()));
                    }
                }
                map.values_mut().for_each(walk);
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

const HIDDEN: &str = "(not shown in the live preview: config files can hold credentials)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_refused_before_anything_runs() {
        for command in ["move_issue", "answer_ask", "set_blocker", "create_project", "connect_apply", "agent_setup_apply", "sync_now", "approve", ""] {
            let e = run(command, &json!({ "key": "NIT-1", "state": "Approved", "id": 1, "answer": "yes" })).unwrap_err();
            assert!(e.to_string().contains("read-only"), "{command}: {e}");
        }
    }

    #[test]
    fn config_files_never_reach_the_page() {
        let plan = json!([{ "agent": "claude", "found": true, "changes": [
            { "target": "~/.claude.json", "action": "update", "why": "MCP server", "before": "{\"token\":\"secret\"}", "after": "{\"token\":\"secret\"}" }
        ] }]);
        let out = without_files(plan).to_string();
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("~/.claude.json") && out.contains("update") && out.contains(HIDDEN));
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
