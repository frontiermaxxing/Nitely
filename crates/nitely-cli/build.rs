//! Records the commit this nitely is built from, so it can tell when it is older than the Nitely
//! code it would submit or land (crates/nitely-core/src/install.rs, `stale_build`).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=NITELY_COMMIT={commit}");
    // Build again when HEAD moves: its own file, the branch it names, and packed refs. Paths come
    // from git, so this works in a worktree too.
    let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
    let watched = [Some("HEAD".to_string()), head_ref, Some("packed-refs".to_string())];
    for name in watched.into_iter().flatten() {
        if let Some(path) = git(&["rev-parse", "--git-path", &name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
