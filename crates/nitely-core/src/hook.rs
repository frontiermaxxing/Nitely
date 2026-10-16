//! Turns an agent's hook payload into a [`NewEvent`]. Only tool names, commands and file paths
//! are kept, never prompt text or file contents.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::db::NewEvent;

const SUMMARY_MAX: usize = 160;

/// Claude Code hook input (stdin JSON): `session_id`, `cwd`, `hook_event_name`, and for tool
/// events `tool_name` and `tool_input`.
pub fn from_claude(input: &Value) -> Result<NewEvent> {
    let str_field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let session = str_field("session_id");
    let cwd = str_field("cwd");
    if session.is_empty() || cwd.is_empty() {
        bail!("hook input has no session_id or cwd");
    }
    let event = str_field("hook_event_name");

    let (kind, summary, state) = match event {
        "SessionStart" => ("session_start", Some("Session started".to_string()), "working"),
        "UserPromptSubmit" => ("prompt", Some("Got a prompt".to_string()), "working"),
        "PreToolUse" | "PostToolUse" => (
            "tool",
            Some(tool_summary(str_field("tool_name"), input.get("tool_input"))),
            "working",
        ),
        "Notification" => ("waiting", Some("Waiting for input".to_string()), "waiting"),
        "Stop" => ("turn_end", None, "idle"),
        "SubagentStop" => ("subagent_end", None, "working"),
        "SessionEnd" => ("session_end", None, "ended"),
        other => ("other", Some(other.to_string()), "working"),
    };

    Ok(NewEvent {
        session_id: format!("claude:{session}"),
        vendor: "claude".into(),
        cwd: cwd.to_string(),
        kind: kind.into(),
        summary: summary.map(|s| truncate(&s)),
        state: state.into(),
    })
}

fn tool_summary(tool: &str, input: Option<&Value>) -> String {
    let field = |k: &str| {
        input
            .and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(|s| s.lines().next().unwrap_or("").to_string())
    };
    let detail = match tool {
        "Bash" | "PowerShell" => field("command"),
        "Edit" | "Write" | "Read" | "MultiEdit" | "NotebookEdit" => field("file_path"),
        "Grep" | "Glob" => field("pattern"),
        "Task" | "Agent" => field("description"),
        _ => None,
    };
    match detail {
        Some(d) if !d.is_empty() => format!("{tool}: {d}"),
        _ => tool.to_string(),
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= SUMMARY_MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(SUMMARY_MAX - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_tool_keeps_first_line_of_command() {
        let e = from_claude(&json!({
            "session_id": "s1", "cwd": "C:\\wt\\and-1", "hook_event_name": "PostToolUse",
            "tool_name": "Bash", "tool_input": { "command": "npm run test:gate\necho done" }
        }))
        .unwrap();
        assert_eq!(e.session_id, "claude:s1");
        assert_eq!(e.summary.as_deref(), Some("Bash: npm run test:gate"));
        assert_eq!(e.state, "working");
    }

    #[test]
    fn prompt_text_is_not_recorded() {
        let e = from_claude(&json!({
            "session_id": "s1", "cwd": "/w", "hook_event_name": "UserPromptSubmit",
            "prompt": "secret plans"
        }))
        .unwrap();
        assert!(!e.summary.unwrap().contains("secret"));
    }

    #[test]
    fn stop_means_idle_not_ended() {
        let e = from_claude(&json!({"session_id": "s", "cwd": "/w", "hook_event_name": "Stop"})).unwrap();
        assert_eq!(e.state, "idle");
    }
}
