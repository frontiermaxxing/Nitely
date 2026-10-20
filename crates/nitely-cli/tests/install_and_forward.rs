//! A `nitely mcp` that keeps running while a new build is installed over it answers its next tool
//! call from the new build.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn temp() -> PathBuf {
    let dir = nitely_core::paths::unique_temp("nitely-forward", "");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

/// `built` at `to` as a hard link, so no new copy of the program is written for the virus scanner
/// to read in full (NIT-289); a copy where links cannot be made. Only for files nothing writes into.
fn link_or_copy(built: &Path, to: &Path) {
    if std::fs::hard_link(built, to).is_err() {
        std::fs::copy(built, to).unwrap();
    }
}

fn call(stdin: &mut impl Write, stdout: &mut impl BufRead, id: u64, method: &str, params: Value) -> Value {
    writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn a_running_mcp_server_forwards_to_a_newly_installed_build() {
    let dir = temp();
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let installed = dir.join("bin").join(exe);
    let built = Path::new(env!("CARGO_BIN_EXE_nitely"));
    std::fs::copy(built, &installed).unwrap();

    let mut server = Command::new(&installed)
        .arg("mcp")
        .env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", dir.join("home"))
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = server.stdin.take().unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    call(&mut stdin, &mut stdout, 1, "initialize", json!({ "protocolVersion": "2025-06-18", "clientInfo": { "name": "claude-code" } }));
    let ask = json!({ "name": "get_answer", "arguments": { "id": 41 } });

    let before = call(&mut stdin, &mut stdout, 2, "tools/call", ask.clone());
    assert!(before["result"]["content"][0]["text"].as_str().unwrap().contains("no ask 41"), "{before}");
    assert!(before["result"].get("_meta").is_none(), "nothing newer yet: {before}");

    // A new build: the same program with a byte appended, which both PE and ELF ignore.
    let mut bytes = std::fs::read(built).unwrap();
    bytes.push(0);
    let new_build = dir.join("new-build");
    std::fs::write(&new_build, bytes).unwrap();
    let out = Command::new(built)
        .args(["install", "--from"])
        .arg(&new_build)
        .arg("--to")
        .arg(&installed)
        .env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", dir.join("home"))
        .env("NITELY_INSTALL_WITHIN", &dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["replaced"], true, "{report}");

    let after = call(&mut stdin, &mut stdout, 3, "tools/call", ask);
    assert!(after["result"]["content"][0]["text"].as_str().unwrap().contains("no ask 41"), "{after}");
    assert!(after["result"]["_meta"]["nitely_forwarded"].is_string(), "answered by the new build: {after}");

    drop(stdin);
    server.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn source_install_refuses_a_live_land_before_rebuilding_or_replacing_the_target() {
    let dir = temp();
    let repo = dir.join("repo");
    let manifest = repo.join("crates/nitely-cli");
    std::fs::create_dir_all(manifest.join("src")).unwrap();
    std::fs::write(manifest.join("src/main.rs"), "fn main() {}").unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);
    let lock = repo.join(".git/nitely-land.lock");
    let holder = format!("APP-1 pid {} since now", std::process::id());
    std::fs::write(&lock, &holder).unwrap();
    let target = dir.join("installed");
    std::fs::write(&target, "keep this").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nitely"))
        .args(["install", "--to"]).arg(&target)
        .env("CARGO_MANIFEST_DIR", &manifest)
        .env_remove("NITELY_INSTALL_LOCK")
        .env("NITELY_HOME", dir.join("home"))
        .env("NITELY_INSTALL_WITHIN", &dir)
        .output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("another land or install"), "{out:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep this");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), holder);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn install_without_a_target_skips_its_own_folders_on_path() {
    // As under `cargo run`, which puts target/release/deps (holding a copy of the build) and
    // target/release first on PATH.
    let dir = temp();
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let built = Path::new(env!("CARGO_BIN_EXE_nitely"));
    let release = dir.join("target").join("release");
    std::fs::create_dir_all(release.join("deps")).unwrap();
    link_or_copy(built, &release.join(exe));
    link_or_copy(built, &release.join("deps").join(exe));
    let installed = dir.join("bin").join(exe);
    std::fs::write(&installed, b"an old nitely").unwrap();
    let path = std::env::join_paths([release.join("deps"), release.clone(), dir.join("bin")]).unwrap();
    // A data folder of its own: install reads the database it would serve, never the person's.
    let out = Command::new(release.join(exe)).arg("install").env("PATH", path).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", dir.join("home")).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["replaced"], true, "{report}");
    assert_eq!(std::fs::read(&installed).unwrap(), std::fs::read(built).unwrap(), "the build is now the one on PATH");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn install_from_a_file_beside_the_installed_one_replaces_it() {
    let dir = temp();
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let built = Path::new(env!("CARGO_BIN_EXE_nitely"));
    let installed = dir.join("bin").join(exe);
    std::fs::write(&installed, b"an old nitely").unwrap();
    let new_build = dir.join("bin").join(if cfg!(windows) { "nitely-new.exe" } else { "nitely-new" });
    link_or_copy(built, &new_build);
    let out = Command::new(built).args(["install", "--from"]).arg(&new_build).env("PATH", dir.join("bin")).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", dir.join("home")).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(&installed).unwrap(), std::fs::read(built).unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

/// An install of a build that could not open the person's database (one a build with a migration
/// it lacks upgraded) is refused, and the nitely in place stays (NIT-239).
#[test]
fn install_refuses_a_build_that_could_not_open_the_database() {
    let dir = temp();
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let built = Path::new(env!("CARGO_BIN_EXE_nitely"));
    let installed = dir.join("bin").join(exe);
    std::fs::write(&installed, b"the newer nitely").unwrap();
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    // The database as a newer build left it: one migration more than this build has.
    drop(nitely_core::Db::open(&home.join("nitely.db")).unwrap());
    let db = rusqlite::Connection::open(home.join("nitely.db")).unwrap();
    db.execute("INSERT INTO schema_migration (name, applied_at, applied_by) VALUES ('20991231-later', 0, '9.9.0')", []).unwrap();
    drop(db);
    let out = Command::new(built).args(["install", "--to"]).arg(&installed).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", &home).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && said.contains("could not open your database") && said.contains("20991231-later"), "{said}");
    assert_eq!(std::fs::read(&installed).unwrap(), b"the newer nitely", "the nitely in place stays");

    // Another build given with --from is asked: a copy of this one answers for itself that it
    // cannot open the database.
    let other = dir.join(if cfg!(windows) { "other.exe" } else { "other" });
    std::fs::copy(built, &other).unwrap();
    let out = Command::new(built).args(["install", "--from"]).arg(&other).arg("--to").arg(&installed).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", &home).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && said.contains("20991231-later"), "{said}");
    // One that cannot say (not a nitely, or one older than the check) goes in only with --force.
    let mute = dir.join("mute.bin");
    std::fs::write(&mute, b"not a program").unwrap();
    let out = Command::new(built).args(["install", "--from"]).arg(&mute).arg("--to").arg(&installed).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", &home).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("--force"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(&installed).unwrap(), b"the newer nitely");
    let out = Command::new(built).args(["install", "--force", "--from"]).arg(&mute).arg("--to").arg(&installed).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", &home).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(&installed).unwrap(), b"not a program", "--force installs it");

    // And no test installs outside its own folder, whatever PATH finds.
    let out = Command::new(built).args(["install", "--to"]).arg(std::env::temp_dir().join("elsewhere").join(exe)).env_remove("CARGO_MANIFEST_DIR").env("NITELY_HOME", dir.join("empty")).env("NITELY_INSTALL_WITHIN", &dir).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("NITELY_INSTALL_WITHIN allows only"), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_dir_all(&dir).ok();
}
