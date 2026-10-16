//! Tells the app when something it shows changed, so views update the moment an agent acts
//! instead of on the next poll. Two cheap checks, run often:
//! - SQLite's `data_version`, which moves whenever another connection commits (a hook, the MCP
//!   server, the CLI, a sync);
//! - the modification times of each watched repo's refs, HEADs and worktree list, and of the config
//!   file (a branch moved, a commit landed, a worktree came or went, a repo was added).
//!
//! The index is left out on purpose: reading a worktree's status can rewrite it, which would make
//! every refresh trigger the next. Uncommitted edits are picked up by the app's slow fallback poll.

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::db::Db;
use crate::paths;

pub struct Watcher {
    db: Db,
    data_version: i64,
    files: u64,
}

impl Watcher {
    pub fn new() -> Result<Watcher> {
        let db = Db::open_default()?;
        let data_version = db.data_version()?;
        Ok(Watcher { db, data_version, files: files_fingerprint() })
    }

    /// True when anything changed since the last call.
    pub fn changed(&mut self) -> Result<bool> {
        let version = self.db.data_version()?;
        let files = files_fingerprint();
        let changed = version != self.data_version || files != self.files;
        self.data_version = version;
        self.files = files;
        Ok(changed)
    }
}

impl Db {
    /// Moves whenever another connection commits to the database.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self.conn.pragma_query_value(None, "data_version", |r| r.get(0))?)
    }
}

/// What moves when the config or any watched repo's refs, HEADs or worktree list change.
pub fn repos_fingerprint() -> u64 {
    files_fingerprint()
}

fn files_fingerprint() -> u64 {
    let mut h = DefaultHasher::new();
    stamp(&paths::config_path(), &mut h);
    if let Ok(config) = Config::load() {
        for repo in &config.repos {
            for path in git_paths(Path::new(&repo.root)) {
                stamp(&path, &mut h);
            }
        }
    }
    h.finish()
}

/// What moves when branches, commits or worktrees change, for a repo's main checkout.
fn git_paths(root: &Path) -> Vec<PathBuf> {
    let git = root.join(".git");
    let mut out = vec![git.join("HEAD"), git.join("packed-refs"), git.join("worktrees")];
    dirs_under(&git.join("refs"), 4, &mut out);
    if let Ok(entries) = std::fs::read_dir(git.join("worktrees")) {
        for e in entries.flatten() {
            out.push(e.path().join("HEAD"));
        }
    }
    out
}

/// A directory's mtime changes when an entry in it is added or removed; a ref file's when it moves.
fn dirs_under(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    out.push(dir.to_path_buf());
    if depth == 0 {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs_under(&p, depth - 1, out);
            } else {
                out.push(p);
            }
        }
    }
}

fn stamp(path: &Path, h: &mut DefaultHasher) {
    path.hash(h);
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    modified.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).hash(h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_version_moves_on_another_connections_commit() {
        let path = std::env::temp_dir().join(format!("nitely-watch-{}-{}.db", std::process::id(), crate::db::now()));
        let watcher_db = Db::open(&path).unwrap();
        let before = watcher_db.data_version().unwrap();
        assert_eq!(watcher_db.data_version().unwrap(), before, "no change, no move");
        Db::open(&path).unwrap().create_project("NIT", "Nitely").unwrap();
        assert_ne!(watcher_db.data_version().unwrap(), before);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn opening_the_database_is_not_a_change() {
        let path = std::env::temp_dir().join(format!("nitely-watch-open-{}-{}.db", std::process::id(), crate::db::now()));
        let watcher_db = Db::open(&path).unwrap();
        let before = watcher_db.data_version().unwrap();
        for _ in 0..3 {
            let other = Db::open(&path).unwrap();
            other.projects().unwrap();
        }
        assert_eq!(watcher_db.data_version().unwrap(), before, "every app command opens the database; that must not wake the watcher");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ref_changes_change_the_fingerprint() {
        let repo = std::env::temp_dir().join(format!("nitely-watch-repo-{}-{}", std::process::id(), crate::db::now()));
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let print = |paths: Vec<PathBuf>| {
            let mut h = DefaultHasher::new();
            paths.iter().for_each(|p| stamp(p, &mut h));
            h.finish()
        };
        let a = print(git_paths(&repo));
        std::thread::sleep(std::time::Duration::from_millis(20));
        git(&["branch", "claude/nit-2-x"]);
        assert_ne!(print(git_paths(&repo)), a, "a new branch is seen");
        std::fs::remove_dir_all(&repo).ok();
    }
}
