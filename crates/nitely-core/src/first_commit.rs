//! A repo's first commit, made from Add a project (NIT-685). Tasks start from the integration
//! branch, so files never committed would be missing from every agent's worktree, and an empty
//! commit does not help. `preview` says what would go in: how many files and how much, the largest
//! folders, and what should not go in (build and dependency folders, files over 50 MB, likely
//! secrets), with a .gitignore built from what is there when the repo has none. `make` writes that
//! .gitignore (the lines the person kept), sets the git identity for this repo when git has none,
//! and makes one local commit. It never pushes.
use anyhow::{bail, Result};
use serde::Serialize;
use std::path::Path;

use crate::playbook::git;

/// A file at least this big is flagged: a commit keeps it in the history for good.
pub const LARGE_BYTES: u64 = 50 * 1024 * 1024;

/// Build output and dependency folders: rebuilt or reinstalled, never committed.
const BUILD_DIRS: &[&str] = &["node_modules", "target", "dist", ".venv", "venv", "__pycache__", "bin", "obj", ".next"];

/// Why a path should not go into the first commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// Build output or installed dependencies.
    Build,
    /// Over `LARGE_BYTES`.
    Large,
    /// Looks like a key, a password file or an environment file.
    Secret,
}

/// What `path` (relative, `/`-separated) is, if it should stay out: a folder by its name, a file by
/// its name or its size. A sample environment file (`.env.example`) is meant to be committed.
pub fn classify(path: &str, size: u64, is_dir: bool) -> Option<Reason> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_lowercase();
    if is_dir {
        return BUILD_DIRS.contains(&name).then_some(Reason::Build);
    }
    let sample = [".example", ".sample", ".template"].iter().any(|s| lower.ends_with(s));
    let secret = !sample
        && (lower == ".env"
            || lower.starts_with(".env.")
            || [".pem", ".key", ".p12", ".pfx"].iter().any(|e| lower.ends_with(e))
            || lower.starts_with("id_rsa")
            || lower.starts_with("id_ed25519")
            || lower.starts_with("id_ecdsa")
            || lower.contains("credentials"));
    if secret {
        return Some(Reason::Secret);
    }
    (size >= LARGE_BYTES).then_some(Reason::Large)
}

/// The .gitignore line that keeps a flagged path out: a folder's name, a secret's pattern, or a
/// large file's own path.
pub fn ignore_line(path: &str, reason: Reason) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_lowercase();
    match reason {
        Reason::Build => format!("{name}/"),
        Reason::Large => format!("/{path}"),
        Reason::Secret => {
            if lower == ".env" || lower.starts_with(".env.") {
                ".env*".into()
            } else if let Some(ext) = [".pem", ".key", ".p12", ".pfx"].iter().find(|e| lower.ends_with(**e)) {
                format!("*{ext}")
            } else if lower.starts_with("id_") {
                format!("{}*", name.split('.').next().unwrap_or(name))
            } else {
                format!("/{path}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Flagged {
    pub path: String,
    pub reason: Reason,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    /// Files that would go in, and their size.
    pub files: usize,
    pub bytes: u64,
    /// The biggest top-level folders that would go in, biggest first: (folder, bytes).
    pub largest: Vec<(String, u64)>,
    /// What should stay out.
    pub flagged: Vec<Flagged>,
    /// A .gitignore to write, line by line, when the repo has none; None when it has one.
    pub gitignore: Option<Vec<String>>,
    /// git has no user.name or user.email for this repo: the commit needs them.
    pub needs_identity: bool,
    /// The branch the commit goes on.
    pub branch: String,
}

/// What the first commit of the repo at `root` would hold. Refuses a repo that has commits.
pub fn preview(root: &Path) -> Result<Preview> {
    if git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        bail!("{} has commits already", root.display());
    }
    let branch = git(root, &["symbolic-ref", "--short", "HEAD"])?.trim().to_string();
    let existing = std::fs::read_to_string(root.join(".gitignore")).ok();
    let (mut files, mut bytes, mut flagged) = (0usize, 0u64, Vec::new());
    let mut top: std::collections::BTreeMap<String, u64> = Default::default();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if let Some(reason) = classify(&path, 0, true) {
                    flagged.push(Flagged { path, reason, bytes: 0 });
                } else {
                    pending.push((e.path(), path));
                }
                continue;
            }
            if let Some(reason) = classify(&path, meta.len(), false) {
                flagged.push(Flagged { path, reason, bytes: meta.len() });
                continue;
            }
            files += 1;
            bytes += meta.len();
            if let Some((first, _)) = path.split_once('/') {
                *top.entry(first.to_string()).or_default() += meta.len();
            }
        }
    }
    flagged.sort_by(|a, b| a.path.cmp(&b.path));
    let mut largest: Vec<(String, u64)> = top.into_iter().collect();
    largest.sort_by(|a, b| b.1.cmp(&a.1));
    largest.truncate(5);
    let gitignore = existing.is_none().then(|| {
        let mut lines: Vec<String> = Vec::new();
        for f in &flagged {
            let line = ignore_line(&f.path, f.reason);
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        lines
    });
    let identity = |key: &str| git(root, &["config", key]).map(|v| !v.trim().is_empty()).unwrap_or(false);
    Ok(Preview { files, bytes, largest, flagged, gitignore, needs_identity: !(identity("user.name") && identity("user.email")), branch })
}

/// Makes the first commit of the repo at `root`: writes `gitignore` (the lines the person kept)
/// when the repo has no .gitignore, sets `identity` (name, email) for this repo only, adds what is
/// there, leaves out each path in `exclude`, and commits "Initial commit". Never pushes.
pub fn make(root: &Path, gitignore: Option<&[String]>, exclude: &[String], identity: Option<(&str, &str)>) -> Result<String> {
    if git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        bail!("{} has commits already", root.display());
    }
    if let Some((name, email)) = identity.filter(|(n, e)| !n.trim().is_empty() && !e.trim().is_empty()) {
        git(root, &["config", "user.name", name.trim()])?;
        git(root, &["config", "user.email", email.trim()])?;
    }
    let has = |key: &str| git(root, &["config", key]).map(|v| !v.trim().is_empty()).unwrap_or(false);
    if !(has("user.name") && has("user.email")) {
        bail!("git has no name and email to commit with: give them, and they are set for this repo only");
    }
    let path = root.join(".gitignore");
    if let Some(lines) = gitignore.filter(|l| !l.is_empty()) {
        if !path.exists() {
            std::fs::write(&path, format!("{}\n", lines.join("\n")))?;
        }
    }
    git(root, &["add", "--all"])?;
    for p in exclude {
        git(root, &["rm", "-r", "--cached", "--quiet", "--ignore-unmatch", "--", p])?;
    }
    let staged = git(root, &["diff", "--cached", "--name-only"])?.lines().filter(|l| !l.trim().is_empty()).count();
    if staged == 0 {
        bail!("nothing to commit: the folder has no files that would go in");
    }
    git(root, &["commit", "--quiet", "-m", "Initial commit"])?;
    let branch = git(root, &["symbolic-ref", "--short", "HEAD"]).map(|b| b.trim().to_string()).unwrap_or_else(|_| "the branch".into());
    Ok(format!("Made the first commit on {branch}: {staged} file{}. It was not pushed.", if staged == 1 { "" } else { "s" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What stays out of a first commit: build and dependency folders, files over 50 MB, likely
    /// secrets; a sample environment file and ordinary source go in (NIT-685).
    #[test]
    fn build_folders_large_files_and_secrets_stay_out() {
        assert_eq!(classify("node_modules", 0, true), Some(Reason::Build));
        assert_eq!(classify("app/target", 0, true), Some(Reason::Build));
        assert_eq!(classify("src", 0, true), None);
        assert_eq!(classify("assets/video.mp4", LARGE_BYTES, false), Some(Reason::Large));
        assert_eq!(classify("assets/logo.png", LARGE_BYTES - 1, false), None);
        for secret in [".env", ".env.local", "deploy/server.pem", "tls.key", "id_rsa", "id_ed25519.pub", "aws-credentials.json"] {
            assert_eq!(classify(secret, 10, false), Some(Reason::Secret), "{secret}");
        }
        assert_eq!(classify(".env.example", 10, false), None, "a sample is meant to be committed");
        assert_eq!(classify("src/main.rs", 10, false), None);
        assert_eq!(ignore_line("web/node_modules", Reason::Build), "node_modules/");
        assert_eq!(ignore_line(".env.local", Reason::Secret), ".env*");
        assert_eq!(ignore_line("deploy/server.pem", Reason::Secret), "*.pem");
        assert_eq!(ignore_line("id_rsa.pub", Reason::Secret), "id_rsa*");
        assert_eq!(ignore_line("assets/video.mp4", Reason::Large), "/assets/video.mp4");
    }

    /// The preview and the commit on a real fresh repo: flagged paths are proposed for .gitignore
    /// and stay out of the commit, which goes on main with the identity given, and is not pushed.
    #[test]
    fn a_fresh_repo_gets_a_first_commit_without_what_should_stay_out() {
        let dir = crate::paths::unique_temp("nitely-first-commit", "");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::write(dir.join(".env"), "TOKEN=1").unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]).unwrap();
        // Isolated from the machine's own identity, so the identity step is exercised.
        git(&dir, &["config", "user.name", ""]).unwrap();
        git(&dir, &["config", "user.email", ""]).unwrap();
        let p = preview(&dir).unwrap();
        assert_eq!((p.files, p.branch.as_str()), (1, "main"));
        assert_eq!(p.gitignore.as_deref(), Some(&[".env*".to_string(), "node_modules/".to_string()][..]));
        assert!(p.needs_identity);
        assert!(make(&dir, p.gitignore.as_deref(), &[], None).unwrap_err().to_string().contains("name and email"));
        let said = make(&dir, p.gitignore.as_deref(), &[], Some(("You", "you@example.com"))).unwrap();
        assert!(said.contains("on main") && said.contains("not pushed"), "{said}");
        let committed = git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert_eq!(committed.lines().collect::<Vec<_>>(), [".gitignore", "src/main.rs"]);
        assert!(preview(&dir).is_err() && make(&dir, None, &[], None).is_err(), "only a repo with no commits");
        std::fs::remove_dir_all(&dir).ok();
    }
}
