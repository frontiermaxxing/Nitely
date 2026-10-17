use super::*;
use crate::issues::NewIssue;
use std::cell::RefCell;

/// A tracker that records what it was asked to do.
#[derive(Default)]
struct Fake {
    issues: RefCell<Vec<HostIssue>>,
    comments: RefCell<Vec<(i64, String)>>,
    /// The token may delete issues (admin).
    can_delete: bool,
    /// Issue numbers the host refuses to update, as if deleted there.
    gone: Vec<i64>,
}

impl Host for Fake {
    fn name(&self) -> &'static str {
        "GitHub"
    }
    fn repo(&self) -> &str {
        "someone/app"
    }
    fn check(&self) -> Result<()> {
        Ok(())
    }
    fn create(&self, issue: &HostIssue) -> Result<(i64, String)> {
        self.issues.borrow_mut().push(issue.clone());
        let n = self.issues.borrow().len() as i64;
        Ok((n, format!("https://github.com/someone/app/issues/{n}")))
    }
    fn update(&self, number: i64, issue: &HostIssue) -> Result<()> {
        if self.gone.contains(&number) {
            bail!("GitHub 404: Not Found");
        }
        self.issues.borrow_mut()[number as usize - 1] = issue.clone();
        Ok(())
    }
    fn comment(&self, number: i64, body: &str) -> Result<i64> {
        self.comments.borrow_mut().push((number, body.to_string()));
        Ok(self.comments.borrow().len() as i64)
    }
    fn delete_comment(&self, _number: i64, id: i64) -> Result<()> {
        self.comments.borrow_mut()[id as usize - 1].1 = "<deleted>".into();
        Ok(())
    }
    fn delete_issue(&self, number: i64) -> Result<()> {
        if !self.can_delete {
            bail!("needs admin");
        }
        self.issues.borrow_mut()[number as usize - 1].title = "<deleted>".into();
        Ok(())
    }
}

const ROOT: &str = r"C:\code\app";

fn roots() -> Vec<String> {
    vec![ROOT.to_string(), format!("{WORKTREES}C:\\code\\app-worktrees")]
}

fn db() -> (Db, std::path::PathBuf) {
    let dir = crate::paths::unique_temp("nitely-mirror", "");
    let db = Db::open(&dir.join("t.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    (db, dir)
}

fn new(title: &str, body: &str) -> NewIssue {
    NewIssue { project: "APP".into(), title: title.into(), body: body.into(), kind: "feature".into(), source: "claude".into(), evidence: "none yet".into(), priority: 3, allow_similar: true, ..Default::default() }
}

#[test]
fn local_paths_and_agent_ids_go_out_neutral() {
    let roots = roots();
    let text = r"Seen by claude:c:/code/app-worktrees/app-3 in C:\code\app-worktrees\app-3-x\src\lib.rs and C:\code\app\docs\a.md; also C:\Users\me\notes.txt and /Users/me/secret.txt";
    assert_eq!(sanitize_with_home(text, &roots, None), "Seen by claude in src/lib.rs and docs/a.md; also <local path> and <local path>");
    // Folders with spaces, and the home folder whole, even where no path follows it.
    let home = Some(r"C:\Users\Sam Lee");
    assert_eq!(
        sanitize_with_home(r"Notes in C:\Users\Sam Lee\Private\notes.txt and in C:\Data\My Files\b.txt, from C:\Users\Sam Lee\My Files\c.txt.", &roots, home),
        "Notes in <local path> and in <local path>, from <local path>."
    );
    // A sibling that only starts with the repo's name is not one of its worktrees; any Unix path goes.
    assert_eq!(sanitize_with_home(r"See C:\code\app-secrets\archive\notes.txt", &roots, None), "See <local path>");
    assert_eq!(sanitize_with_home("In /srv/customer/notes.txt and /Volumes/Private/x.md", &roots, None), "In <local path> and <local path>");
    assert_eq!(sanitize_with_home("By claude:c:/Users/Sam Lee/app-worktrees/x today", &roots, None), "By claude today");
    // Network shares, a `..` out of the repo, and a file name with a space.
    assert_eq!(sanitize_with_home(r"On \\server\secret share\notes.txt now", &roots, None), "On <local path> now");
    assert_eq!(sanitize_with_home(r"C:\code\app\..\app-secrets\x\notes.txt", &roots, None), "<local path>");
    assert_eq!(sanitize_with_home(r"Read C:\Docs\report v2.pdf and more", &roots, None), "Read <local path> and more");
    // Extended and device paths, and a forward-slash share, go out whole; a URL is left alone.
    assert_eq!(sanitize_with_home(r"See \\?\C:\Data\customer-private\notes.txt now", &roots, None), "See <local path> now");
    assert_eq!(sanitize_with_home(r"See \\?\UNC\server\share\x.txt", &roots, None), "See <local path>");
    assert_eq!(sanitize_with_home(r"See \\.\pipe\private-pipe", &roots, None), "See <local path>");
    assert_eq!(sanitize_with_home("See //server/private share/x.txt or https://example.com/a/b", &roots, None), "See <local path> or https://example.com/a/b");
    // A `..` through a worktree folder or a task folder is not inside the repo either.
    assert_eq!(sanitize_with_home(r"C:\code\app-worktrees\..\app-secrets\x\notes.txt", &roots, None), "<local path>");
    assert_eq!(sanitize_with_home(r"C:\code\app-worktrees\app-3\..\..\private\notes.txt", &roots, None), "<local path>");
    assert_eq!(sanitize_with_home(r"C:\code\app-worktrees\app-3\src\main.rs", &roots, None), "src/main.rs");
    // A folder with a space, in a path that is not under the home folder.
    assert_eq!(sanitize_with_home(r"C:\Users\Sam Lee\Private\notes.txt", &roots, None), "<local path>");
}

#[test]
fn sensitive_issues_stay_private_and_private_terms_hold_one_back() {
    let roots = roots();
    let none = PrivateTerms::default();
    let issue = |f: &dyn Fn(&mut IssueRecord)| {
        let mut i = IssueRecord { key: "APP-1".into(), project: Some("APP".into()), title: "Add export".into(), body: "Export the list.".into(), kind: Some("feature".into()), priority: 3, state: "Todo".into(), state_type: "unstarted".into(), owner: "agent".into(), ..Default::default() };
        f(&mut i);
        i
    };
    let private = |d: Decision| matches!(d, Decision::Private(_));
    assert!(private(decide(&issue(&|_| {}), true, &none, &roots).unwrap()), "marked private");
    assert!(private(decide(&issue(&|i| i.owner = "human".into()), false, &none, &roots).unwrap()));
    assert!(private(decide(&issue(&|i| i.labels = vec!["security".into()]), false, &none, &roots).unwrap()));
    assert!(private(decide(&issue(&|i| (i.priority, i.kind) = (1, Some("bug".into()))), false, &none, &roots).unwrap()));
    let terms = PrivateTerms { all: vec!["export".into()], ..Default::default() };
    assert_eq!(decide(&issue(&|_| {}), false, &terms, &roots).unwrap(), Decision::Held("a private term (term 1 of the list for every project) is in its text".into()));
    let Decision::Publish(out) = decide(&issue(&|i| i.labels = vec!["ios".into(), "c:/users/sam/private".into()]), false, &none, &roots).unwrap() else { panic!() };
    assert_eq!(out.labels, ["feature", "priority: medium", "ios"], "a label holding a path is left out");
    assert!(out.body.ends_with("Tracked in Nitely as APP-1 (Todo, feature, medium priority). Nitely is where it is decided; comments here are not read."), "{}", out.body);
}

#[test]
fn syncs_new_issues_updates_comments_and_withdraws() {
    let (mut db, dir) = db();
    let roots = roots();
    let terms = PrivateTerms::default();
    let agent = Actor::Agent("claude:x".into());
    let old = db.create_issue(&new("Older work", "Filed before the mirror."), &agent).unwrap();
    assert!(db.set_mirror("APP", Some("none"), &agent).is_err(), "an agent cannot turn it on");
    db.set_mirror("APP", Some("none"), &Actor::Human).unwrap();
    // The mirror's start is a whole second; the issue filed after it is new.
    db.conn.execute("UPDATE mirror_project SET enabled_at = enabled_at - 5", []).unwrap();
    db.conn.execute("UPDATE issue SET created_at = '2000-01-01T00:00:00Z' WHERE key = ?1", [&old.key]).unwrap();
    let a = db.create_issue(&new("Add export", r"Export from C:\code\app\src\list.rs."), &agent).unwrap();
    let fake = Fake::default();
    let mut report = MirrorReport::default();
    let run = |db: &Db, report: &mut MirrorReport| sync_project(db, &fake, "APP", &terms, &roots, &mut 30, report).unwrap();
    run(&db, &mut report);
    assert_eq!(report.created, 1, "only the new issue, with backfill none");
    assert!(fake.issues.borrow()[0].body.starts_with("Export from src/list.rs."));
    let m = db.issue_mirror(&a.key).unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.number), ("mirrored", Some(1)));

    // Nothing changed: nothing sent. A handoff becomes a comment; a status line does not.
    run(&db, &mut report);
    assert_eq!(report.updated, 0);
    db.add_note(&a.key, "Half done; the CSV part is left.", "handoff", &agent).unwrap();
    db.add_note(&a.key, "reading code", "status", &agent).unwrap();
    run(&db, &mut report);
    assert_eq!(fake.comments.borrow().as_slice(), [(1, "**Handoff note:** Half done; the CSV part is left.".to_string())]);

    // Done closes it as completed, with a comment.
    db.move_issue(&a.key, "Done", Some("landed"), None, &agent).unwrap();
    run(&db, &mut report);
    assert_eq!(fake.issues.borrow()[0].closed, Some("completed"));
    assert!(fake.comments.borrow().iter().any(|(_, c)| c == "**Done** in Nitely: landed"));

    // Made private after it was published: the text and the comments come down.
    db.set_issue_private(&a.key, true, &Actor::Human).unwrap();
    run(&db, &mut report);
    run(&db, &mut report);
    assert!(fake.issues.borrow()[0].title.contains("withdrawn") && !fake.issues.borrow()[0].body.contains("Export"));
    assert!(fake.comments.borrow().iter().all(|(_, c)| c == "<deleted>"), "{:?}", fake.comments.borrow());
    let m = db.issue_mirror(&a.key).unwrap().unwrap();
    assert_eq!(m.status, "exposed", "not reported as withdrawn while its history is readable");
    assert!(m.reason.unwrap().contains("delete the issue there"), "the person is told what is left");
    assert!(report.errors.iter().any(|e| e.contains("edit history")), "{:?}", report.errors);

    // A backfill of all publishes the older issue too.
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    run(&db, &mut report);
    assert_eq!(db.issue_mirror(&old.key).unwrap().unwrap().status, "mirrored");
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn comments_read_for_outsiders() {
    let roots = roots();
    assert_eq!(
        comment_for("submit", "Review by codex passed; ready to land. Pull request: https://x/pr/4", Some("https://x/pr/4"), &roots).as_deref(),
        Some("**Review:** Review by codex passed; ready to land. (https://x/pr/4)")
    );
    assert_eq!(comment_for("issue_moved", "Todo → In Progress", None, &roots), None);
    assert_eq!(comment_for("issue_moved", "In Progress → Canceled: covered by APP-9", None, &roots).as_deref(), Some("**Canceled** in Nitely: covered by APP-9"));
    assert_eq!(comment_for("status", "reading", None, &roots), None);
}

#[test]
fn a_number_from_another_repo_is_not_written_to() {
    let (mut db, dir) = db();
    let roots = roots();
    let agent = Actor::Agent("claude:x".into());
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    let a = db.create_issue(&new("Add export", "Export the list."), &agent).unwrap();
    // It was published on a repo the project's origin no longer points at.
    db.save_mirror(&a.key, "mirrored", None, Some(("GitHub", "someone/old-app", 7, "https://github.com/someone/old-app/issues/7")), Some("x")).unwrap();
    let fake = Fake::default();
    let mut report = MirrorReport::default();
    sync_project(&db, &fake, "APP", &PrivateTerms::default(), &roots, &mut 30, &mut report).unwrap();
    assert_eq!((report.created, report.updated), (1, 0), "published anew here; issue 7 of the old repo is left alone");
    let m = db.issue_mirror(&a.key).unwrap().unwrap();
    assert_eq!(m.url.as_deref(), Some("https://github.com/someone/app/issues/1"));
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_rekey_keeps_private_marks_and_removing_a_project_forgets_the_mirror() {
    let (mut db, dir) = db();
    let agent = Actor::Agent("claude:x".into());
    let a = db.create_issue(&new("Add export", "Export the list."), &agent).unwrap();
    db.set_issue_private(&a.key, true, &Actor::Human).unwrap();
    db.set_mirror("APP", Some("open"), &Actor::Human).unwrap();
    db.rekey_project("APP", "APX").unwrap();
    assert!(db.issue_mirror("APX-1").unwrap().unwrap().marked_private, "the private mark follows the new key");
    assert!(db.mirror_setting("APX").unwrap().is_some());
    db.remove_project("APX", true).unwrap();
    db.create_project("APX", "Again").unwrap();
    assert!(db.mirror_setting("APX").unwrap().is_none(), "a project made again starts with the mirror off");
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_token_that_may_delete_takes_a_withdrawn_issue_down_whole_and_one_refusal_does_not_stop_the_rest() {
    let (mut db, dir) = db();
    let roots = roots();
    let agent = Actor::Agent("claude:x".into());
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    let a = db.create_issue(&new("Add export", "Export the list."), &agent).unwrap();
    let b = db.create_issue(&new("Fix sorting", "Sort by date."), &agent).unwrap();
    let fake = Fake { can_delete: true, gone: vec![1], ..Default::default() };
    let mut report = MirrorReport::default();
    let run = |db: &Db, report: &mut MirrorReport| sync_project(db, &fake, "APP", &PrivateTerms::default(), &roots, &mut 30, report).unwrap();
    run(&db, &mut report);
    // Issue 1 was deleted on the host: its update fails, and issue 2 still updates.
    db.move_issue(&a.key, "Backlog", None, None, &agent).unwrap();
    db.move_issue(&b.key, "Backlog", None, None, &agent).unwrap();
    run(&db, &mut report);
    assert!(report.errors.iter().any(|e| e.starts_with(&a.key)), "{:?}", report.errors);
    assert_eq!(report.updated, 1);
    // With admin rights, withdrawing deletes the issue, edit history and all.
    db.set_issue_private(&b.key, true, &Actor::Human).unwrap();
    run(&db, &mut report);
    assert_eq!(fake.issues.borrow()[1].title, "<deleted>");
    assert!(db.issue_mirror(&b.key).unwrap().unwrap().reason.unwrap().contains("deleted on GitHub"));
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn turning_the_mirror_off_stops_a_sync_on_its_way() {
    let (mut db, dir) = db();
    let roots = roots();
    let agent = Actor::Agent("claude:x".into());
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    for t in ["Add export", "Fix sorting", "Speed up search"] {
        db.create_issue(&new(t, "Body."), &agent).unwrap();
    }
    /// A host on which the first create turns the mirror off, as the person might mid-sync.
    struct Stopper<'a> {
        db: &'a Db,
        fake: Fake,
    }
    impl Host for Stopper<'_> {
        fn name(&self) -> &'static str { "GitHub" }
        fn repo(&self) -> &str { "someone/app" }
        fn check(&self) -> Result<()> { Ok(()) }
        fn create(&self, issue: &HostIssue) -> Result<(i64, String)> {
            self.db.set_mirror("APP", None, &Actor::Human).unwrap();
            self.fake.create(issue)
        }
        fn update(&self, n: i64, issue: &HostIssue) -> Result<()> { self.fake.update(n, issue) }
        fn comment(&self, n: i64, body: &str) -> Result<i64> { self.fake.comment(n, body) }
        fn delete_comment(&self, n: i64, id: i64) -> Result<()> { self.fake.delete_comment(n, id) }
        fn delete_issue(&self, n: i64) -> Result<()> { self.fake.delete_issue(n) }
    }
    let host = Stopper { db: &db, fake: Fake::default() };
    let mut report = MirrorReport::default();
    sync_project(&db, &host, "APP", &PrivateTerms::default(), &roots, &mut 30, &mut report).unwrap();
    assert_eq!(report.created, 1, "nothing more once it is off");
    drop(host);
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_publication_left_on_an_old_repo_is_reported_not_hidden() {
    let (mut db, dir) = db();
    let agent = Actor::Agent("claude:x".into());
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    let a = db.create_issue(&new("Add export", "Export the list."), &agent).unwrap();
    db.save_mirror(&a.key, "mirrored", None, Some(("GitHub", "someone/old-app", 7, "https://github.com/someone/old-app/issues/7")), Some("x")).unwrap();
    db.set_issue_private(&a.key, true, &Actor::Human).unwrap();
    let fake = Fake::default();
    sync_project(&db, &fake, "APP", &PrivateTerms::default(), &roots(), &mut 30, &mut MirrorReport::default()).unwrap();
    let m = db.issue_mirror(&a.key).unwrap().unwrap();
    assert_eq!(m.status, "exposed");
    assert!(m.reason.unwrap().contains("#7 on GitHub someone/old-app"));
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_publication_still_up_on_an_old_repo_stays_reported() {
    let (mut db, dir) = db();
    let agent = Actor::Agent("claude:x".into());
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    let a = db.create_issue(&new("Add export", "Export the list."), &agent).unwrap();
    // A withdrawal on the old repo that could not delete it, then the origin moved.
    db.save_mirror(&a.key, "exposed", Some("x"), Some(("GitHub", "someone/old-app", 7, "https://github.com/someone/old-app/issues/7")), Some("x")).unwrap();
    db.set_issue_private(&a.key, true, &Actor::Human).unwrap();
    let fake = Fake::default();
    sync_project(&db, &fake, "APP", &PrivateTerms::default(), &roots(), &mut 30, &mut MirrorReport::default()).unwrap();
    let m = db.issue_mirror(&a.key).unwrap().unwrap();
    assert_eq!(m.status, "exposed");
    assert!(m.reason.unwrap().contains("#7 on GitHub someone/old-app"));
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_copy_left_on_an_old_repo_is_remembered_after_publishing_anew() {
    let (mut db, dir) = db();
    let agent = Actor::Agent("claude:x".into());
    db.set_mirror("APP", Some("all"), &Actor::Human).unwrap();
    let a = db.create_issue(&new("Add export", "Export the list."), &agent).unwrap();
    db.save_mirror(&a.key, "mirrored", None, Some(("GitHub", "someone/old-app", 7, "https://github.com/someone/old-app/issues/7")), Some("x")).unwrap();
    let fake = Fake::default();
    // Published anew on the project's repo now.
    sync_project(&db, &fake, "APP", &PrivateTerms::default(), &roots(), &mut 30, &mut MirrorReport::default()).unwrap();
    assert_eq!(db.issue_mirror(&a.key).unwrap().unwrap().status, "mirrored");
    // Made private: withdrawn here, and the old copy still said.
    db.set_issue_private(&a.key, true, &Actor::Human).unwrap();
    sync_project(&db, &fake, "APP", &PrivateTerms::default(), &roots(), &mut 30, &mut MirrorReport::default()).ok();
    let m = db.issue_mirror(&a.key).unwrap().unwrap();
    assert_eq!(m.status, "exposed");
    assert!(m.reason.unwrap().contains("#7 on GitHub someone/old-app"));
    drop(db);
    std::fs::remove_dir_all(dir).ok();
}
