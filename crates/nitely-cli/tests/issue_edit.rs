//! `nitely issue edit` checks what it is given before it writes anything: a bad priority leaves
//! the title and body as they were (NIT-326).

use nitely_core::issues::{Actor, NewIssue};
use std::process::Command;

#[test]
fn a_bad_priority_leaves_the_whole_edit_undone() {
    let base = nitely_core::paths::unique_temp("nitely-issue-edit", "");
    std::fs::create_dir_all(&base).unwrap();
    let db = nitely_core::Db::open(&base.join("nitely.db")).unwrap();
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
