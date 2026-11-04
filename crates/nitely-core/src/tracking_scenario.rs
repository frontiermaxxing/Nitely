//! Tracking mode end to end (NIT-739), on a throwaway repo with a remote: agents hand in Markdown
//! changes and they land with no lane run and no reviewer, while the person's own approval paths,
//! rebase conflicts and the switch back to Development behave as before.
use crate::issues::{Actor, NewIssue};
use crate::playbook::{self, PLAYBOOK_PATH};
use crate::project_modes::{Mode, Options};
use crate::test_repo::git as g;
use crate::Db;

// A gate and a reviewer that would both fail: in Tracking neither may run.
const PLAYBOOK: &str = "[worktree]\ndir = \"../wt/{key}\"\n[gate]\ncommands = [\"exit 1\"]\nretries = 0\n[review]\nreviewer = { \"*\" = \"fake\" }\ncommands = { fake = [\"exit 1\"] }\n[[review.flags]]\nname = \"SOURCES\"\nquestion = \"Does it cite a source it does not link?\"\n[[approval.human_paths]]\npattern = \"^budget/\"\nreason = \"the budget is the person's\"\n";

fn issue(db: &mut Db, title: &str) -> crate::issues::IssueRecord {
    db.create_issue(&NewIssue { project: "RES".into(), title: title.into(), body: format!("{title}: a research note."), evidence: "Asked for by the team.".into(), source: "claude".into(), kind: "feature".into(), allow_similar: true, ..Default::default() }, &Actor::Human).unwrap()
}

const SECRET: [u8; 32] = [7; 32];

fn lane_runs(db: &Db) -> i64 {
    db.conn.query_row("SELECT COUNT(*) FROM lane_run", [], |r| r.get(0)).unwrap()
}

/// Every review event anywhere: a reviewer started, finished or failed, of a change, an item, a
/// batch or main, and every review a reviewer wrote. Not the person's change of review mode, nor
/// an issue's declared review level.
fn review_events(db: &Db) -> Vec<String> {
    let mut q = db.conn.prepare("SELECT type || ': ' || COALESCE(summary, '') FROM event WHERE (type LIKE '%review%' AND type NOT IN ('project_review_mode_changed', 'review_level_changed')) OR summary LIKE 'Review by%'
        UNION ALL SELECT 'review by ' || reviewer FROM review WHERE reviewer <> ''").unwrap();
    q.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

/// Writes `files` in the worktree and commits them.
fn commit(wt: &std::path::Path, files: &[(&str, &str)], message: &str) {
    for (path, text) in files {
        let path = wt.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    g(wt, &["add", "-A"]);
    g(wt, &["commit", "-q", "-m", message]);
}

fn submit(db: &mut Db, config: &crate::Config, wt: &str, actor: &Actor) -> crate::submit::Submitted {
    let opts = crate::submit::SubmitOptions { dir: wt, issue_key: None, skip_review: false, local_only: false, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
    crate::submit::submit(db, config, &opts, actor).unwrap()
}

#[test]
fn a_tracking_project_lands_markdown_with_no_lane_run_and_no_review() {
    let base = crate::test_repo::copy("tracking-scenario", |base| {
        crate::test_repo::init(&base.join("repo"), &[(PLAYBOOK_PATH, PLAYBOOK), ("notes/plan.md", "# Plan\n\nOpen question.\n")]);
        crate::test_repo::add_remote(&base.join("repo"), &base.join("remote.git"));
    });
    let (repo, remote) = (base.join("repo"), base.join("remote.git"));
    let mut db = Db::open(&base.join("t.db")).unwrap();
    db.create_project("RES", "Research").unwrap();
    let agent = Actor::Agent("claude:test".into());
    let config = crate::Config::default();

    // Only the person makes a project Tracking, in one step, with the revision checks modes use.
    assert!(db.set_project_review_mode("RES", Mode::Tracking, Options::default(), 0, &agent).is_err(), "an agent cannot");
    let mode = db.set_project_review_mode("RES", Mode::Tracking, Options::default(), 0, &Actor::Human).unwrap();
    assert!(mode.tracking());

    // start_task, commit, submit: it lands on main as it is handed in.
    let a = issue(&mut db, "Survey of sources");
    let started = playbook::start_task_light(&db, &repo, &a, "claude", &agent).unwrap();
    let wt = std::path::PathBuf::from(&started.worktree);
    commit(&wt, &[("notes/survey.md", "# Survey\n\n- First source.\n")], "Survey (RES-1)");
    let out = submit(&mut db, &config, &started.worktree, &agent);
    assert_eq!(db.issue(&a.key).unwrap().unwrap().state, "Done", "{}", out.next);
    assert!(out.pr_url.is_none() && out.pr_error.is_none(), "no pull request: {:?}", out.pr_error);
    assert_eq!(g(&remote, &["show", "main:notes/survey.md"]), "# Survey\n\n- First source.");

    // quick_start, quick_land: no size limit, and no review after landing.
    let b = db.create_issue(&crate::quick::new_issue("RES", "Long literature notes", "claude"), &agent).unwrap();
    let started = crate::quick::start(&db, &repo, &b, "claude", &agent).unwrap();
    let long = "- A finding worth keeping.\n".repeat(300);
    // A new file at the root, one in a new directory, and a binary: none of it stops the quick path.
    let qwt = std::path::Path::new(&started.worktree);
    std::fs::create_dir_all(qwt.join("sources/2026")).unwrap();
    std::fs::write(qwt.join("summary.md"), &long).unwrap();
    std::fs::write(qwt.join("sources/2026/scan.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
    let quick = crate::quick::land(&mut db, &config, &started.worktree, None, &agent).unwrap();
    assert!(quick.small && quick.landed, "{quick:?}");
    assert_eq!(db.issue(&b.key).unwrap().unwrap().state, "Done");
    assert_eq!(g(&remote, &["cat-file", "-t", "main:sources/2026/scan.bin"]), "blob");
    assert!(crate::quick_review::waiting(&db, "RES", &repo).unwrap().is_empty(), "nothing waits for a review after landing");

    assert_eq!(lane_runs(&db), 0, "no gate ran in a lane");
    assert_eq!(review_events(&db), Vec::<String>::new(), "no reviewer was called");

    // The person's own approval rules still apply: a budget change waits for them.
    let c = issue(&mut db, "Budget estimate");
    let started = playbook::start_task_light(&db, &repo, &c, "claude", &agent).unwrap();
    commit(std::path::Path::new(&started.worktree), &[("budget/estimate.md", "# Estimate\n")], "Budget (RES-3)");
    submit(&mut db, &config, &started.worktree, &agent);
    assert_eq!(db.issue(&c.key).unwrap().unwrap().state, "Needs You");
    // Once the person approves it, Nitely lands it: the agent was told it need not land.
    crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
    db.move_issue(&c.key, "Approved", None, None, &Actor::Human).unwrap();
    db.sign_approval(&c.key, &SECRET).unwrap();
    let landed = crate::land::land_on_approval(&mut db, &config, &c.key, &repo.to_string_lossy()).unwrap().expect("Nitely lands an approved Tracking change");
    assert!(landed.landed, "{landed:?}");
    assert_eq!(db.issue(&c.key).unwrap().unwrap().state, "Done");

    // A review that waited for its land batch from before the switch is not run now.
    let queued = issue(&mut db, "Queued under Development");
    db.conn.execute("INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, findings, ts) VALUES (?1, 'p1', 'c1', '', ?2, 'agent', '[]', 1)", rusqlite::params![queued.key, crate::review_batch::PENDING]).unwrap();
    let outcome = crate::review_batch::review_pending(&mut db, &repo, &[queued.key.clone()], &agent).unwrap();
    assert_eq!(outcome, vec![(queued.key.clone(), Ok(()))]);
    assert_eq!(db.reviews(&queued.key).unwrap()[0].verdict, "yolo");

    // A rebase conflict still stops the land and goes back to the agent.
    let (d, e) = (issue(&mut db, "Answer the open question"), issue(&mut db, "Reword the open question"));
    let (sd, se) = (playbook::start_task_light(&db, &repo, &d, "claude", &agent).unwrap(), playbook::start_task_light(&db, &repo, &e, "codex", &agent).unwrap());
    commit(std::path::Path::new(&sd.worktree), &[("notes/plan.md", "# Plan\n\nAnswered.\n")], "Answer (RES-4)");
    commit(std::path::Path::new(&se.worktree), &[("notes/plan.md", "# Plan\n\nReworded question.\n")], "Reword (RES-5)");
    submit(&mut db, &config, &sd.worktree, &agent);
    assert_eq!(db.issue(&d.key).unwrap().unwrap().state, "Done");
    submit(&mut db, &config, &se.worktree, &agent);
    let back = db.issue(&e.key).unwrap().unwrap();
    assert_eq!(back.state, "In Progress", "the conflict goes back to the agent");
    assert_eq!(g(&remote, &["show", "main:notes/plan.md"]), "# Plan\n\nAnswered.", "and nothing of it landed");
    assert_eq!((lane_runs(&db), review_events(&db)), (0, vec![]));

    // What landed under Tracking is the baseline: switching back does not check it after the fact.
    let main = g(&repo, &["rev-parse", "main"]);
    let health = crate::verification_backstop::health(&db, &repo).unwrap().expect("a baseline");
    assert_eq!((health.verified_head.as_deref(), health.observed_head.as_str()), (Some(main.as_str()), main.as_str()));

    // Back to Development: the next submit runs the gate again (it fails here) and nothing lands.
    db.set_project_review_mode("RES", Mode::Release, Options::default(), 1, &Actor::Human).unwrap();
    let f = issue(&mut db, "Methods section");
    let started = playbook::start_task_light(&db, &repo, &f, "claude", &agent).unwrap();
    commit(std::path::Path::new(&started.worktree), &[("notes/methods.md", "# Methods\n")], "Methods (RES-6)");
    let out = submit(&mut db, &config, &started.worktree, &agent);
    assert!(!out.gate.passed && out.gate.skipped.is_none(), "the gate ran: {:?}", out.gate.skipped);
    assert_ne!(db.issue(&f.key).unwrap().unwrap().state, "Done");
    drop(db);
    std::fs::remove_dir_all(base).ok();
}
