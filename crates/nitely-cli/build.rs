//! Records the commit this nitely is built from, so it can tell when it is older than the Nitely
//! code it would submit or land (crates/nitely-core/src/install.rs, `stale_build`), and the release
//! channel it is built for, which `nitely --version` prints.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=NITELY_COMMIT={commit}");
    // The release channel the release workflow builds for (docs/releasing.md); "dev" otherwise.
    let channel = std::env::var("NITELY_CHANNEL").unwrap_or_else(|_| "dev".into());
    println!("cargo:rustc-env=NITELY_BUILD_CHANNEL={channel}");
    println!("cargo:rerun-if-env-changed=NITELY_CHANNEL");
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
