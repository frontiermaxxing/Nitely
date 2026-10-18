//! Records the commit this nitely is built from, so it can tell when it is older than the Nitely
//! code it would submit or land (crates/nitely-core/src/install.rs, `stale_build`), and the release
//! channel it is built for, which `nitely --version` prints.

use std::process::Command;

/// git's answer, asked up to three times: on a busy Windows machine starting git can fail without
/// running it (NIT-116), and a question left unanswered here leaves the build blind to commits.
fn git(args: &[&str]) -> Option<String> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200 * attempt));
        }
        let Ok(out) = Command::new("git").args(args).output() else { continue };
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
        }
    }
    None
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=NITELY_COMMIT={commit}");
    // The release channel the release workflow builds for (docs/releasing.md); "dev" otherwise.
    let channel = std::env::var("NITELY_CHANNEL").unwrap_or_else(|_| "dev".into());
    println!("cargo:rustc-env=NITELY_BUILD_CHANNEL={channel}");
    println!("cargo:rerun-if-env-changed=NITELY_CHANNEL");
    // Build again when HEAD moves: its own file, every branch (the folder, so a fast-forward of the
    // branch HEAD names counts whatever this build saw of it), and packed refs. It watched only the
    // branch `symbolic-ref` named, so a build made while that call failed (a detached HEAD, git not
    // starting) kept its stamp through every land after it, and submit refused the install as
    // older than main (NIT-320). Paths come from git, so this works in a worktree too.
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
