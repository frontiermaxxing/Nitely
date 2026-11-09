use std::process::Command;

/// git's answer, or nothing when git cannot say (a source tarball, git not starting).
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    // tauri-build embeds icons/icon.ico in the exe but asks to rerun only when the config changes,
    // so a new icon alone would leave the old one in the build.
    println!("cargo:rerun-if-changed=icons");
    // The commit this app is built from and its time, so it can tell when the installed nitely is
    // newer and land approved changes with that instead of its own older code (NIT-752).
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    let committed_at = if commit.is_empty() { String::new() } else { git(&["show", "-s", "--format=%ct", &commit]).unwrap_or_default() };
    println!("cargo:rustc-env=NITELY_COMMIT={commit}");
    println!("cargo:rustc-env=NITELY_COMMIT_TIME={committed_at}");
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    tauri_build::build()
}
