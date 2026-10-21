//! The UserPromptSubmit hook adds the session's unread updates to its prompt, through the real
//! `nitely hook claude` (NIT-562): told once, and then nothing until something new happens.

use nitely_core::issues::{Actor, NewIssue};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn a_prompt_hears_its_own_send_back_and_the_reviewer_settings_once() {
    let base = nitely_core::paths::unique_temp("nitely-updates-hook", "");
    std::fs::create_dir_all(&base).unwrap();
    let mut db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let me = Actor::Agent("claude:s1".into());
    let issue = db
        .create_issue(&NewIssue { project: "APP".into(), title: "Footer link".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "claude".into(), allow_similar: true, ..Default::default() }, &me)
        .unwrap();
    // A background submit records the review's verdict under the session that submitted it.
    db.log_event(&me, Some(&issue.key), None, "submit", "Review by codex requested changes. Pull request: x").unwrap();
    db.log_event(&Actor::Human, None, None, "reviewer_settings_changed", "Review settings for app: Saved.").unwrap();

    let prompt = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nitely")).args(["hook", "claude"]).current_dir(&base).env("NITELY_HOME", &base).env_remove("NITELY_UPDATES_SCOPE").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let input = serde_json::json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1", "cwd": base.to_string_lossy(), "prompt": "go on" });
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let first = prompt();
    let added: serde_json::Value = serde_json::from_str(first.lines().find(|l| l.contains("additionalContext")).unwrap_or_else(|| panic!("no context added: {first}"))).unwrap();
    let text = added["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(text.contains(&format!("{}: the review asked for changes", issue.key)), "{text}");
    assert!(text.contains("The person changed the reviewer settings"), "{text}");
    assert!(!prompt().contains("additionalContext"), "told once");
    drop(db);
    std::fs::remove_dir_all(base).ok();
}
