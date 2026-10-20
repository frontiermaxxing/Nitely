use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn machine_transport_runs_snapshot_and_records_executor_without_local_lane() {
    let dir = nitely_core::paths::unique_temp("nitely-executor-cli", "");
    let source = dir.join("source");
    let target = dir.join("target");
    let sender = dir.join("sender-home");
    let receiver = dir.join("receiver-home");
    for path in [&source, &sender, &receiver] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::create_dir_all(source.join(".nitely")).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    std::fs::write(
        source.join(".nitely/playbook.toml"),
        "[lanes]\nexecutor='machine:test'\nquick=false\n[worktree]\ndir='../worktrees/{key}'\n",
    )
    .unwrap();
    std::fs::write(source.join("proof.txt"), "old").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "initial"]);
    git(
        &dir,
        &[
            "clone",
            "--quiet",
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    );
    std::fs::write(source.join("proof.txt"), "new snapshot").unwrap();
    let config =
        |home: &Path, root: &Path| {
            std::fs::write(home.join("config.json"),serde_json::to_vec(&serde_json::json!({
            "repos":[{"name":"Acme","root":root,"project":"APP","integration_branch":"main"}],
            "memory_floor_gb":0,"memory_cap_gb":1,"build_slots":1
        })).unwrap()).unwrap();
        };
    config(&sender, &source);
    config(&receiver, &target);
    std::fs::write(
        receiver.join("executors.json"),
        r#"{"receive_projects":["APP"]}"#,
    )
    .unwrap();
    let relay = dir.join("relay.cjs");
    std::fs::write(&relay,"const c=require('child_process').spawn(process.argv[2],['executor','receive','--project','APP'],{env:{...process.env,NITELY_HOME:process.argv[3]},stdio:'inherit'});c.on('exit',code=>process.exit(code??1));").unwrap();
    std::fs::write(sender.join("executors.json"),serde_json::to_vec(&serde_json::json!({"machines":{"test":{
        "command":["node",relay,env!("CARGO_BIN_EXE_nitely"),receiver],"project":"APP","projects":["APP"]
    }}})).unwrap()).unwrap();
    let output = dir.join("out.log");
    let error = dir.join("err.log");
    #[cfg(windows)]
    let shell = "type proof.txt";
    #[cfg(not(windows))]
    let shell = "cat proof.txt";
    let mut child = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .args(["run", "--shell", shell])
        .env("NITELY_HOME", &sender)
        .current_dir(&source)
        .stdout(std::fs::File::create(&output).unwrap())
        .stderr(std::fs::File::create(&error).unwrap())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "isolated executor CLI timed out: {}",
                std::fs::read_to_string(&error).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "{}",
        std::fs::read_to_string(&error).unwrap()
    );
    assert!(std::fs::read_to_string(output)
        .unwrap()
        .contains("new snapshot"));
    let db = rusqlite::Connection::open(sender.join("nitely.db")).unwrap();
    let row: (String, Option<i64>, i64) = db
        .query_row("SELECT executor,lane,exit_code FROM lane_run", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(row, ("machine:test".into(), None, 0));
    assert_eq!(
        std::fs::read_to_string(target.join("proof.txt")).unwrap(),
        "old"
    );
    let failure = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .args(["run", "--shell", "exit 7"])
        .env("NITELY_HOME", &sender)
        .current_dir(&source)
        .output()
        .unwrap();
    assert_eq!(
        failure.status.code(),
        Some(7),
        "remote command's exit code survives transport"
    );
    let last: i64 = db
        .query_row(
            "SELECT exit_code FROM lane_run ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last, 7);
    drop(db);
    let _ = std::fs::remove_dir_all(dir);
}
