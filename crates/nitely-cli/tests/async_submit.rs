//! `nitely submit` hands a branch in and returns at once; a process of its own runs the gate and the
//! review, and `nitely submit-status` brings the result back (NIT-212).

use nitely_core::issues::{Actor, NewIssue};
use nitely_core::playbook;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) {
    let o = playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// A repo whose gate passes and whose reviewer passes everything once the test releases it (writes
/// `release`), a Nitely home, and an issue with a commit in its worktree. Held so, a submit that
/// returned at all did not wait for the review, however slow the machine (NIT-267).
struct Setup {
    base: PathBuf,
    home: PathBuf,
    release: PathBuf,
    worktree: PathBuf,
    key: String,
    db: nitely_core::Db,
}

/// The fake reviewer: it reads its prompt, then answers once `release` exists (giving up after two
/// minutes, so a test that never releases it fails instead of hanging).
fn reviewer(release: &Path) -> String {
    let release = serde_json::to_string(&release.to_string_lossy()).unwrap();
    [
        "const fs = require('fs');",
        &format!("const release = {release};"),
        "const until = Date.now() + 120000;",
        "process.stdin.resume();",
        "process.stdin.on('end', function wait() {",
        "  if (fs.existsSync(release)) console.log('Fine.\\nVERDICT: clean');",
        "  else if (Date.now() > until) process.exit(1);",
        "  else setTimeout(wait, 50);",
        "});",
        "",
    ]
    .join("\n")
}

fn setup(name: &str) -> Setup {
    let base = nitely_core::paths::unique_temp(name, "");
    let home = base.join("home");
    let repo = base.join("repo");
    let release = base.join("release");
    std::fs::create_dir_all(repo.join(".nitely")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("review.js"), reviewer(&release)).unwrap();
    std::fs::write(
        repo.join(playbook::PLAYBOOK_PATH),
        "[worktree]\ndir = \"../wt/{key}\"\n[gate]\ncommands = [\"exit 0\"]\n[review]\nreviewer = { \"*\" = \"claude\" }\ncommands = { claude = [\"node\", \"review.js\"] }\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let mut db = nitely_core::Db::open(&home.join("nitely.db")).unwrap();
    db.create_project("APP", "App").unwrap();
    let actor = Actor::Agent("codex:test".into());
    let issue = db
        .create_issue(&NewIssue { project: "APP".into(), title: "Footer link".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() }, &actor)
        .unwrap();
    db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
    let started = playbook::start_task(&db, &repo, &issue, "codex", &actor).unwrap();
    let worktree = PathBuf::from(&started.worktree);
    std::fs::write(worktree.join("footer.txt"), "x").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-q", "-m", "footer"]);
    Setup { base, home, release, worktree, key: issue.key, db }
}

impl Setup {
    /// The job is handed in and not done: its review is still held.
    fn still_reviewing(&self, id: &str) {
        let job = nitely(self, &["submit-status", id]);
        assert!(matches!(job["state"].as_str(), Some("queued" | "running")), "the job waits for the held review: {job}");
    }

    /// Lets the reviewer answer.
    fn release_review(&self) {
        std::fs::write(&self.release, "").unwrap();
    }
}

fn nitely(s: &Setup, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_nitely")).args(args).current_dir(&s.worktree).env("NITELY_HOME", &s.home).env("NITELY_MEMORY_FLOOR_GB", "0").output().unwrap();
    assert!(out.status.success(), "nitely {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// One MCP tool call, as a running `nitely mcp` forwards it, from the worktree.
fn tool(s: &Setup, name: &str, arguments: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .arg("mcp-call")
        .current_dir(&s.worktree)
        .env("NITELY_HOME", &s.home).env("NITELY_MEMORY_FLOOR_GB", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = json!({ "vendor": "codex", "cwd": s.worktree, "params": { "name": name, "arguments": arguments } });
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let out = child.wait_with_output().unwrap();
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(result.get("isError").is_none(), "{name}: {text}");
    serde_json::from_str(text).unwrap()
}

#[test]
fn disabled_review_async_mcp_submission_still_gates_and_lands() {
    let s = setup("nitely-disabled-review-async");
    let repo = s.base.join("repo");
    let path = repo.join(playbook::PLAYBOOK_PATH);
    let initial = std::fs::read_to_string(&path).unwrap();
    let disabled = initial.replace("[review]\n", "[review]\nenabled = false\nbatch = \"all\"\n");
    std::fs::write(&path, &disabled).unwrap();
    git(&repo, &["commit", "-qam", "Disable automated review"]);
    // The task still has the old enabled playbook; authoritative settings win. Its fake reviewer
    // is never released, so accidentally starting it cannot yield a passing submission.
    let job = tool(&s, "submit", json!({}));
    let id = job["id"].as_i64().unwrap().to_string();
    let done = nitely(&s, &["submit-status", &id, "--wait", "600"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["result"]["review_disabled"], true, "{done}");
    assert_eq!(done["result"]["gate"]["passed"], true);
    assert!(done["result"]["review"].is_null());
    assert_eq!(done["result"]["state"], "In Review");
    assert_eq!(s.db.reviews(&s.key).unwrap()[0].verdict, "disabled");

    std::fs::write(&path, &initial).unwrap();
    git(&repo, &["commit", "-qam", "Enable automated review"]);
    let refused_out = Command::new(env!("CARGO_BIN_EXE_nitely")).args(["land", &s.key, "--no-push", "--keep"])
        .current_dir(&s.worktree).env("NITELY_HOME", &s.home).env("NITELY_MEMORY_FLOOR_GB", "0").output().unwrap();
    assert!(!refused_out.status.success());
    let refused: Value = serde_json::from_slice(&refused_out.stdout).unwrap();
    assert_eq!(refused["landed"], false, "{refused}");
    assert!(refused.to_string().contains("enabled again"), "{refused}");

    std::fs::write(&path, &disabled).unwrap();
    git(&repo, &["commit", "-qam", "Disable automated review again"]);
    let landed = nitely(&s, &["land", &s.key, "--no-push", "--keep"]);
    assert_eq!(landed["landed"], true, "{landed}");
    assert_eq!(s.db.issue(&s.key).unwrap().unwrap().state, "Done");
    std::fs::remove_dir_all(&s.base).ok();
}

#[test]
fn submit_returns_at_once_and_the_background_job_finishes_it() {
    let s = setup("nitely-async-submit");
    // Returned while the review is held, so it did not wait for it.
    let job = nitely(&s, &["submit", "--local"]);
    assert!(matches!(job["state"].as_str(), Some("queued" | "running")), "{job}");
    let id = job["id"].as_i64().unwrap().to_string();
    s.still_reviewing(&id);
    s.release_review();

    let done = nitely(&s, &["submit-status", &id, "--wait", "600"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["result"]["state"], "In Review", "{done}");
    assert_eq!(s.db.issue(&s.key).unwrap().unwrap().state, "In Review");
    std::fs::remove_dir_all(&s.base).ok();
}

#[test]
fn an_agent_hands_in_over_mcp_and_hears_the_result_on_its_next_call() {
    let s = setup("nitely-async-submit-mcp");
    let job = tool(&s, "submit", json!({}));
    assert!(job["next"].as_str().unwrap().contains("Handed in"), "{job}");
    let id = job["id"].as_i64().unwrap();
    s.still_reviewing(&id.to_string());
    s.release_review();

    // Someone else waiting for it (here the CLI) does not use up the agent's notice.
    let done = nitely(&s, &["submit-status", &id.to_string(), "--wait", "600"]);
    assert_eq!(done["state"], "done", "{done}");

    // The agent's next nitely call, of any kind, carries the outcome once.
    let next = tool(&s, "get_issue", json!({ "key": s.key }));
    let notices = next["notices"].as_array().unwrap_or_else(|| panic!("no notice: {next}"));
    assert!(notices[0].as_str().unwrap().starts_with(&format!("{}: submitted, now In Review.", s.key)), "{notices:?}");
    let again = tool(&s, "get_issue", json!({ "key": s.key }));
    assert!(again.get("notices").is_none(), "told once: {again}");

    // submit_status by key answers the same, with the result's next step.
    let status = tool(&s, "submit_status", json!({ "key": s.key }));
    assert_eq!((status["id"].as_i64(), status["state"].as_str()), (Some(id), Some("done")));
    assert!(status["next"].as_str().unwrap().contains("Call land"), "{status}");
    std::fs::remove_dir_all(&s.base).ok();
}
