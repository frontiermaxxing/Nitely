//! `nitely submit` hands a branch in and returns at once; a process of its own runs the gate and the
//! review, and `nitely submit-status` brings the result back (NIT-212).

use nitely_core::issues::{Actor, NewIssue};
use nitely_core::playbook;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) {
    let o = playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// A repo whose gate passes and whose reviewer passes everything after two seconds, a Nitely home,
/// and an issue with a commit in its worktree.
struct Setup {
    base: PathBuf,
    home: PathBuf,
    worktree: PathBuf,
    key: String,
    db: nitely_core::Db,
}

fn setup(name: &str) -> Setup {
    let base = nitely_core::paths::unique_temp(name, "");
    let home = base.join("home");
    let repo = base.join("repo");
    std::fs::create_dir_all(repo.join(".nitely")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("review.js"), "process.stdin.resume();\nprocess.stdin.on('end', () => setTimeout(() => console.log('Fine.\\nVERDICT: clean'), 2000));\n").unwrap();
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
    Setup { base, home, worktree, key: issue.key, db }
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
fn submit_returns_at_once_and_the_background_job_finishes_it() {
    let s = setup("nitely-async-submit");
    let asked = Instant::now();
    let job = nitely(&s, &["submit", "--local"]);
    assert!(asked.elapsed() < Duration::from_secs(2), "submit waited for the review: {:?}", asked.elapsed());
    assert!(matches!(job["state"].as_str(), Some("queued" | "running")), "{job}");
    let id = job["id"].as_i64().unwrap().to_string();

    let done = nitely(&s, &["submit-status", &id, "--wait", "120"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["result"]["state"], "In Review", "{done}");
    assert_eq!(s.db.issue(&s.key).unwrap().unwrap().state, "In Review");
    std::fs::remove_dir_all(&s.base).ok();
}

#[test]
fn an_agent_hands_in_over_mcp_and_hears_the_result_on_its_next_call() {
    let s = setup("nitely-async-submit-mcp");
    let asked = Instant::now();
    let job = tool(&s, "submit", json!({}));
    assert!(asked.elapsed() < Duration::from_secs(2), "submit waited for the review: {:?}", asked.elapsed());
    assert!(job["next"].as_str().unwrap().contains("Handed in"), "{job}");
    let id = job["id"].as_i64().unwrap();

    // Someone else waiting for it (here the CLI) does not use up the agent's notice.
    let done = nitely(&s, &["submit-status", &id.to_string(), "--wait", "120"]);
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
