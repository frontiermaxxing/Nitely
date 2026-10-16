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

/// Codex's `notify` program gets one JSON argument after each turn: `type`
/// (`agent-turn-complete`), `thread-id` and `cwd`, plus the turn's messages, which are not kept.
pub fn from_codex(input: &Value) -> Result<NewEvent> {
    let str_field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let cwd = str_field("cwd");
    if cwd.is_empty() {
        bail!("codex notify payload has no cwd");
    }
    let thread = [str_field("thread-id"), str_field("conversation-id")].into_iter().find(|s| !s.is_empty()).unwrap_or(cwd);
    let (kind, summary, state) = match str_field("type") {
        "agent-turn-complete" => ("turn_end", Some("Turn finished".to_string()), "idle"),
        other => ("other", Some(other.to_string()), "working"),
    };
    Ok(NewEvent {
        session_id: format!("codex:{thread}"),
        vendor: "codex".into(),
        cwd: cwd.into(),
        kind: kind.into(),
        summary: summary.map(|s| truncate(&s)),
        state: state.into(),
    })
}

/// Cursor hook input (stdin JSON): `hook_event_name`, `conversation_id`, `workspace_roots`, and
/// for `afterFileEdit` the `file_path`. Prompt text is not kept.
pub fn from_cursor(input: &Value) -> Result<NewEvent> {
    let str_field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let cwd = input.pointer("/workspace_roots/0").and_then(Value::as_str).unwrap_or("");
    let conversation = str_field("conversation_id");
    if cwd.is_empty() || conversation.is_empty() {
        bail!("cursor hook input has no conversation_id or workspace_roots");
    }
    let (kind, summary, state) = match str_field("hook_event_name") {
        "beforeSubmitPrompt" => ("prompt", Some("Got a prompt".to_string()), "working"),
        "afterFileEdit" => ("tool", Some(format!("Edit: {}", str_field("file_path"))), "working"),
        "beforeShellExecution" => ("tool", Some(format!("Shell: {}", str_field("command").lines().next().unwrap_or(""))), "working"),
        "stop" => ("turn_end", None, "idle"),
        other => ("other", Some(other.to_string()), "working"),
    };
    Ok(NewEvent {
        session_id: format!("cursor:{conversation}"),
        vendor: "cursor".into(),
        cwd: cwd.replace('\\', "/"),
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
        // The issue a Nitely tool acts on, so its events can be traced to this session.
        t if t.starts_with("mcp__nitely__") => field("key"),
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
    fn nitely_tools_name_their_issue_but_not_their_text() {
        let e = from_claude(&json!({
            "session_id": "s1", "cwd": "/w", "hook_event_name": "PostToolUse",
            "tool_name": "mcp__nitely__note", "tool_input": { "key": "NIT-27", "text": "private handoff" }
        }))
        .unwrap();
        assert_eq!(e.summary.as_deref(), Some("mcp__nitely__note: NIT-27"));
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
    fn codex_and_cursor_payloads_map_to_sessions() {
        let e = from_codex(&json!({
            "type": "agent-turn-complete", "thread-id": "t1", "cwd": "/w",
            "input-messages": ["secret"], "last-assistant-message": "secret"
        }))
        .unwrap();
        assert_eq!((e.session_id.as_str(), e.state.as_str()), ("codex:t1", "idle"));
        assert!(!format!("{e:?}").contains("secret"));

        let e = from_cursor(&json!({
            "hook_event_name": "afterFileEdit", "conversation_id": "c1", "workspace_roots": ["C:\\dev\\x"],
            "file_path": "src/a.ts", "edits": [{ "old_string": "secret", "new_string": "secret" }]
        }))
        .unwrap();
        assert_eq!((e.session_id.as_str(), e.cwd.as_str()), ("cursor:c1", "C:/dev/x"));
        assert_eq!(e.summary.as_deref(), Some("Edit: src/a.ts"));
        let e = from_cursor(&json!({ "hook_event_name": "beforeSubmitPrompt", "conversation_id": "c1", "workspace_roots": ["/w"], "prompt": "secret" })).unwrap();
        assert!(!format!("{e:?}").contains("secret"));
    }

    #[test]
    fn stop_means_idle_not_ended() {
        let e = from_claude(&json!({"session_id": "s", "cwd": "/w", "hook_event_name": "Stop"})).unwrap();
        assert_eq!(e.state, "idle");
    }
}
