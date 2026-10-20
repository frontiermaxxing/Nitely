//! `nitely issue edit` checks what it is given before it writes anything: a bad priority leaves
//! the title and body as they were (NIT-326).

use nitely_core::issues::{Actor, NewIssue};
use std::process::Command;

#[test]
fn a_bad_priority_leaves_the_whole_edit_undone() {
    let base = nitely_core::paths::unique_temp("nitely-issue-edit", "");
    std::fs::create_dir_all(&base).unwrap();
    let mut db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let issue = db
        .create_issue(&NewIssue { project: "APP".into(), title: "Footer link".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() }, &Actor::Agent("codex:test".into()))
        .unwrap();
    let edit = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_nitely")).args(["issue", "edit", &issue.key]).args(args).current_dir(&base).env("NITELY_HOME", &base).output().unwrap();

    let out = edit(&["--title", "Changed", "--priority", "5"]);
    assert!(!out.status.success(), "a priority of 5 is refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("priority is 0"), "{}", String::from_utf8_lossy(&out.stderr));
    let after = db.issue(&issue.key).unwrap().unwrap();
    assert_eq!((after.title.as_str(), after.priority), ("Footer link", 0), "nothing of the edit was written");

    let out = edit(&["--title", "Changed", "--priority", "2"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let after = db.issue(&issue.key).unwrap().unwrap();
    assert_eq!((after.title.as_str(), after.priority), ("Changed", 2));
    drop(db);
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn review_declarations_from_cli_are_agent_authored_and_downgrades_write_nothing() {
    let base = nitely_core::paths::unique_temp("nitely-cli-review", "");
    std::fs::create_dir_all(&base).unwrap();
    let db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_nitely")).args(args).current_dir(&base).env("NITELY_HOME", &base).output().unwrap();
    let out = run(&["issue", "create", "--project", "APP", "--title", "Default declaration", "--kind", "feature", "--source", "codex", "--body", "New behavior", "--evidence", "scenario"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let issue = db.issue("APP-1").unwrap().unwrap();
    assert_eq!(issue.review_level.as_deref(), Some("standard"));
    assert_eq!(issue.review_level_by.as_deref(), Some("cli"));
    assert_eq!(issue.review_level_source.as_deref(), Some("author"));
    let out = run(&["issue", "edit", "APP-1", "--review-level", "sensitive"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let issue = db.issue("APP-1").unwrap().unwrap();
    assert_eq!(issue.review_level_source.as_deref(), Some("coordinator"));
    for level in ["inert", "undeclared"] {
        let out = run(&["issue", "edit", "APP-1", "--title", "Wrong title", "--priority", "2", "--review-level", level]);
        assert!(!out.status.success(), "agent CLI must not lower or clear: {out:?}");
        let after = db.issue("APP-1").unwrap().unwrap();
        assert_eq!(after.title, issue.title);
        assert_eq!(after.priority, issue.priority);
        assert_eq!(after.review_level.as_deref(), Some("sensitive"));
    }
    let batch = base.join("batch.json");
    std::fs::write(&batch, r#"[{"ref":"a","title":"Keyboard navigation","kind":"feature","body":"Support the keyboard","evidence":"scenario"}]"#).unwrap();
    let out = run(&["issue", "create-many", batch.to_str().unwrap(), "--project", "APP"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let issue = db.issue("APP-2").unwrap().unwrap();
    assert_eq!(issue.review_level.as_deref(), Some("standard"));
    assert_eq!(issue.review_level_source.as_deref(), Some("author"));
    assert_eq!(issue.review_level_by.as_deref(), Some("cli"));
    drop(db);
    std::fs::remove_dir_all(base).ok();
}
