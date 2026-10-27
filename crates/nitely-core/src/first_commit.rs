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
/// large file's own path. A pattern is lowercase, so a name in any other case gets its own path:
/// git matches case-sensitively where the file system does (NIT-685 review).
pub fn ignore_line(path: &str, reason: Reason) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_lowercase();
    match reason {
        Reason::Build => format!("{}/", literal(name)),
        Reason::Large => format!("/{}", literal(path)),
        Reason::Secret if name != lower => format!("/{}", literal(path)),
        Reason::Secret => {
            if lower == ".env" {
                ".env".into()
            } else if lower.starts_with(".env.") {
                ".env.*".into()
            } else if let Some(ext) = [".pem", ".key", ".p12", ".pfx"].iter().find(|e| lower.ends_with(**e)) {
                format!("*{ext}")
            } else if lower.starts_with("id_") {
                format!("{}*", literal(name.split('.').next().unwrap_or(name)))
            } else {
                format!("/{}", literal(path))
            }
        }
    }
}

/// `path` as a .gitignore line that matches only itself: git's pattern characters (`*`, `?`, `[`,
/// `]`, `\`) escaped, and a leading `!` or `#` too, which would otherwise negate or comment the line.
pub fn literal(path: &str) -> String {
    let mut out = String::new();
    for (i, c) in path.chars().enumerate() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') || (i == 0 && matches!(c, '!' | '#')) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The lines that keep environment templates in when `.env.*` keeps the real ones out: a sample is
/// meant to be committed (`classify` lets it through, so the .gitignore must too).
const ENV_TEMPLATES: &[&str] = &["!.env.example", "!.env.sample", "!.env.template"];

#[derive(Debug, Clone, Serialize)]
pub struct Flagged {
    pub path: String,
    pub reason: Reason,
    pub bytes: u64,
    /// The .gitignore line that keeps it out (`ignore_line`), so the page can tell which flagged
    /// paths a line it unticks lets in.
    pub line: String,
}

/// Whether the staged path `path` (`/`-separated) is one `classify` keeps out: a file it flags, or a
/// file inside a folder it flags. A .gitignore cannot be trusted with that alone: a nested one can
/// re-include what the root one excludes (NIT-685 review).
pub fn kept_out(path: &str, size: u64) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    (1..parts.len()).any(|n| classify(&parts[..n].join("/"), 0, true).is_some()) || classify(path, size, false).is_some()
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

/// The .gitignore proposed for `flagged`: name patterns first, then the lines that keep environment
/// templates in, then each file kept out by its own path. Git takes the last line that matches, so
/// a template that is itself flagged (over 50 MB, say) stays out (NIT-685 review).
pub fn proposed_gitignore(flagged: &[Flagged]) -> Vec<String> {
    let (mut patterns, mut paths): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for f in flagged {
        let line = f.line.clone();
        let list = if line.starts_with('/') { &mut paths } else { &mut patterns };
        if !list.contains(&line) {
            list.push(line);
        }
    }
    if patterns.iter().any(|l| l == ".env.*") {
        patterns.extend(ENV_TEMPLATES.iter().map(|l| l.to_string()));
    }
    patterns.extend(paths);
    patterns
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
                    let line = ignore_line(&path, reason);
                    flagged.push(Flagged { path, reason, bytes: 0, line });
                } else {
                    pending.push((e.path(), path));
                }
                continue;
            }
            if let Some(reason) = classify(&path, meta.len(), false) {
                let line = ignore_line(&path, reason);
                flagged.push(Flagged { path, reason, bytes: meta.len(), line });
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
    let gitignore = existing.is_none().then(|| proposed_gitignore(&flagged));
    let identity = |key: &str| git(root, &["config", key]).map(|v| !v.trim().is_empty()).unwrap_or(false);
    Ok(Preview { files, bytes, largest, flagged, gitignore, needs_identity: !(identity("user.name") && identity("user.email")), branch })
}

/// Makes the first commit of the repo at `root`: writes `gitignore` (the lines the person kept)
/// when the repo has no .gitignore, sets `identity` (name, email) for this repo only, adds what is
/// there, and commits "Initial commit". Whatever `kept_out` flags stays out, whatever the
/// .gitignore files say, unless it is in `include` (a flagged path, or a folder above it, that the
/// person chose to commit after all). Never pushes.
pub fn make(root: &Path, gitignore: Option<&[String]>, include: &[String], identity: Option<(&str, &str)>) -> Result<String> {
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
    // Whatever was staged before (a `.env` added by hand, say) goes back out of the index, so what
    // is decided here alone is what the commit holds (NIT-685 review). The repo has no commit, so
    // nothing else is lost: the files stay as they are on disk. read-tree empties it whatever state
    // each entry is in (staged, then edited again, say).
    git(root, &["read-tree", "--empty"])?;
    git(root, &["add", "--all"])?;
    // What the person chose to commit after all goes in even where a .gitignore of the repo's own
    // keeps it out (an unticked `bin` with `bin/` in it): add --all alone would skip it (NIT-685 review).
    for batch in include.chunks(200) {
        let mut args = vec!["--literal-pathspecs", "add", "--force", "--"];
        args.extend(batch.iter().map(String::as_str));
        git(root, &args)?;
    }
    let chosen = |p: &str| include.iter().any(|i| p == i || p.starts_with(&format!("{i}/")));
    // The staged paths as they are: NUL-separated and unquoted, since git quotes a name with
    // non-ASCII characters in its default output (NIT-685 review).
    let out: Vec<String> = git(root, &["-c", "core.quotePath=false", "diff", "--cached", "--name-only", "--no-renames", "-z"])?
        .split('\0')
        .map(str::to_string)
        .filter(|p| !p.is_empty() && !chosen(p))
        .filter(|p| kept_out(p, std::fs::metadata(root.join(p)).map(|m| m.len()).unwrap_or(0)))
        .collect();
    // Each one named literally: a path with `[` or `*` in it is not a pattern here.
    for batch in out.chunks(200) {
        let mut args = vec!["--literal-pathspecs", "rm", "--cached", "--quiet", "--ignore-unmatch", "--"];
        args.extend(batch.iter().map(String::as_str));
        git(root, &args)?;
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
        assert_eq!(ignore_line(".env", Reason::Secret), ".env");
        assert_eq!(ignore_line(".env.local", Reason::Secret), ".env.*");
        // A name not in lowercase is kept out by its own path, which matches whatever the case rules.
        assert_eq!(ignore_line(".ENV", Reason::Secret), "/.ENV");
        assert_eq!(ignore_line("deploy/server.PEM", Reason::Secret), "/deploy/server.PEM");
        // A path line matches only that path: git's pattern characters are escaped.
        assert_eq!(ignore_line("certs/[prod]/server.PEM", Reason::Secret), r"/certs/\[prod\]/server.PEM");
        assert_eq!(literal("!important*.bin"), r"\!important\*.bin");
        // A flagged template stays out: its own path line comes after the template exceptions.
        let flag = |path: &str, reason| Flagged { path: path.into(), reason, bytes: 0, line: ignore_line(path, reason) };
        assert!(kept_out("web/node_modules/pkg/index.js", 1) && kept_out("certs/server.pem", 1) && !kept_out("src/main.rs", 1));
        let lines = proposed_gitignore(&[flag(".env.example", Reason::Large), flag(".env.local", Reason::Secret)]);
        assert_eq!(lines.last().map(String::as_str), Some("/.env.example"), "{lines:?}");
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
        std::fs::write(dir.join(".env.local"), "TOKEN=2").unwrap();
        std::fs::write(dir.join(".env.example"), "TOKEN=").unwrap();
        // A nested .gitignore that re-includes a key does not get it into the commit.
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::write(dir.join("certs/server.pem"), "KEY").unwrap();
        std::fs::write(dir.join("certs/.gitignore"), "!server.pem\n").unwrap();
        // Nor do keys whose names git would quote, or read as a pattern.
        std::fs::write(dir.join("certs/clé.pem"), "KEY").unwrap();
        std::fs::write(dir.join("certs/[x].pem"), "KEY").unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]).unwrap();
        // A secret the person staged before connecting stays out all the same, and a file staged
        // and then edited again does not stop the commit.
        git(&dir, &["add", "--force", ".env", "src/main.rs"]).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() { println!(\"edited\"); }\n").unwrap();
        // Isolated from the machine's own identity, so the identity step is exercised.
        git(&dir, &["config", "user.name", ""]).unwrap();
        git(&dir, &["config", "user.email", ""]).unwrap();
        let p = preview(&dir).unwrap();
        assert_eq!((p.files, p.branch.as_str()), (3, "main"));
        let lines = [".env", ".env.*", "*.pem", "node_modules/", "!.env.example", "!.env.sample", "!.env.template"].map(String::from);
        assert_eq!(p.gitignore.as_deref(), Some(&lines[..]));
        assert!(p.needs_identity);
        assert!(make(&dir, p.gitignore.as_deref(), &[], None).unwrap_err().to_string().contains("name and email"));
        let said = make(&dir, p.gitignore.as_deref(), &[], Some(("You", "you@example.com"))).unwrap();
        assert!(said.contains("on main") && said.contains("not pushed"), "{said}");
        let committed = git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert_eq!(committed.lines().collect::<Vec<_>>(), [".env.example", ".gitignore", "certs/.gitignore", "src/main.rs"], "the template goes in; the staged .env and the re-included key do not");
        assert!(preview(&dir).is_err() && make(&dir, None, &[], None).is_err(), "only a repo with no commits");
        std::fs::remove_dir_all(&dir).ok();

        // With a .gitignore of its own that keeps nothing out, the backend alone keeps the keys
        // out, names git would quote and names with pattern characters included.
        let dir = crate::paths::unique_temp("nitely-first-commit", "");
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::write(dir.join(".gitignore"), "").unwrap();
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        std::fs::write(dir.join("certs/clé.pem"), "KEY").unwrap();
        std::fs::write(dir.join("certs/[x].pem"), "KEY").unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]).unwrap();
        make(&dir, None, &[], Some(("You", "you@example.com"))).unwrap();
        let committed = git(&dir, &["-c", "core.quotePath=false", "ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert_eq!(committed.lines().collect::<Vec<_>>(), [".gitignore", "README.md"]);
        std::fs::remove_dir_all(&dir).ok();

        // A flagged folder the person chose goes in, though the repo's own .gitignore keeps it out.
        let dir = crate::paths::unique_temp("nitely-first-commit", "");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join(".gitignore"), "bin/
").unwrap();
        std::fs::write(dir.join("bin/tool"), "#!/bin/sh").unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]).unwrap();
        make(&dir, None, &["bin".to_string()], Some(("You", "you@example.com"))).unwrap();
        let committed = git(&dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert_eq!(committed.lines().collect::<Vec<_>>(), [".gitignore", "bin/tool"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
