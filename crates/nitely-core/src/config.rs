use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::paths;
use crate::playbook::Playbook;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    /// The main checkout. Linked worktrees are found through `git worktree list`.
    pub root: String,
    /// Branch tasks are cut from and land on (`win-dev` / `mac-dev` for Aera).
    pub integration_branch: String,
    /// The project (issue prefix) this repo's work belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = paths::config_path();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.dedupe_roots(&paths::data_dir());
        Ok(config)
    }

    /// Keeps one entry per repo root, the last written. Older versions replaced repos by name only,
    /// so adding a watched repo under another name left a stale entry that lookups by root found first.
    fn dedupe_roots(&mut self, data_dir: &Path) {
        let mut kept: Vec<RepoConfig> = Vec::new();
        for repo in self.repos.drain(..).rev() {
            match kept.iter().find(|k| paths::normalize(&k.root) == paths::normalize(&repo.root)) {
                // When its playbook can't be carried over, the old entry stays, still ahead of the new
                // one for lookups, so the repo keeps its rules.
                Some(newer) if adopt_playbook(data_dir, &repo.name, &newer.name).is_err() => kept.push(repo),
                Some(_) => {}
                None => kept.push(repo),
            }
        }
        kept.reverse();
        self.repos = kept;
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::config_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    /// The repo that `dir` is in: its main checkout or any of its linked worktrees, wherever
    /// they live on disk.
    pub fn repo_for_dir(&self, dir: &str) -> Option<&RepoConfig> {
        let main = crate::git::main_root(dir)?;
        self.repos
            .iter()
            .find(|r| paths::normalize(&r.root) == paths::normalize(&main))
    }

    /// Adds a repo, or replaces the one with the same name or the same root.
    /// Fails, changing nothing, when the playbook kept for a replaced entry can't be carried over.
    pub fn upsert_repo(&mut self, repo: RepoConfig) -> Result<()> {
        self.upsert_repo_in(&paths::data_dir(), repo)
    }

    fn upsert_repo_in(&mut self, data_dir: &Path, repo: RepoConfig) -> Result<()> {
        let root = paths::normalize(&repo.root);
        for old in self.repos.iter().filter(|r| r.name != repo.name && paths::normalize(&r.root) == root) {
            adopt_playbook(data_dir, &old.name, &repo.name)?;
        }
        self.repos.retain(|r| r.name != repo.name && paths::normalize(&r.root) != root);
        self.repos.push(repo);
        Ok(())
    }
}

/// When a repo's entry is replaced by one under another name, the playbook Nitely keeps for the old
/// name goes with it; otherwise the repo would silently fall back to the defaults (no gate, no
/// human approval). A playbook already kept for the new name wins.
/// The copy is written beside and renamed, so a failure never leaves a partial playbook behind.
fn adopt_playbook(data_dir: &Path, from: &str, to: &str) -> Result<()> {
    let (old, new) = (Playbook::stored_path_in(data_dir, from), Playbook::stored_path_in(data_dir, to));
    if old == new || !old.exists() || new.exists() {
        return Ok(());
    }
    let tmp = new.with_extension("nitely-tmp");
    let copied = fs::copy(&old, &tmp).and_then(|_| fs::rename(&tmp, &new));
    if copied.is_err() {
        fs::remove_file(&tmp).ok();
    }
    copied.with_context(|| format!("carrying the playbook {} over to {}", old.display(), new.display()))
}

/// The project an agent in `cwd` works on: an explicit choice, then `NITELY_PROJECT`, then the
/// project of the repo `cwd` is in, then the only project if there is exactly one.
pub fn resolve_project(config: &Config, db: &crate::Db, explicit: Option<&str>, cwd: &str) -> Result<String> {
    let chosen = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("NITELY_PROJECT").ok().filter(|p| !p.is_empty()))
        .or_else(|| config.repo_for_dir(cwd).and_then(|r| r.project.clone()));
    if let Some(key) = chosen {
        return db
            .project(&key)?
            .map(|p| p.key)
            .with_context(|| format!("no project {key}; create it with `nitely project create {key} <name>`"));
    }
    let projects = db.projects()?;
    match projects.as_slice() {
        [only] => Ok(only.key.clone()),
        [] => anyhow::bail!("no projects yet; create one with `nitely project create NIT Nitely`"),
        _ => anyhow::bail!(
            "several projects and {cwd} is not in a repo linked to one; pass a project, or link the repo with `nitely repo add <path> --project <KEY>`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, root: &str) -> RepoConfig {
        RepoConfig { name: name.into(), root: root.into(), integration_branch: "main".into(), project: None }
    }

    fn names(config: &Config) -> Vec<&str> {
        config.repos.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn one_entry_per_root_and_the_playbook_moves_with_it() {
        let data = std::env::temp_dir().join(format!("nitely-config-{}-{}", std::process::id(), crate::db::now()));
        std::fs::create_dir_all(data.join("playbooks")).unwrap();
        let kept = |name: &str| Playbook::stored_path_in(&data, name);
        std::fs::write(kept("Aera"), "[branch]\nintegration = \"win-dev\"\n").unwrap();

        let mut config = Config { repos: vec![repo("Aera", r"C:\Dev\AeraChromium"), repo("Nitely", "/dev/Nitely")] };
        config.upsert_repo_in(&data, repo("AeraChromium", "C:/Dev/AeraChromium/")).unwrap();
        assert_eq!(names(&config), vec!["Nitely", "AeraChromium"]);
        assert!(std::fs::read_to_string(kept("AeraChromium")).unwrap().contains("win-dev"), "the gate and approval rules are kept");

        // A playbook already kept for the surviving name is not overwritten.
        std::fs::write(kept("Old"), "old").unwrap();
        let mut stale = Config { repos: vec![repo("Old", r"C:\Dev\AeraChromium"), repo("Nitely", "/dev/Nitely"), repo("AeraChromium", "C:/Dev/AeraChromium")] };
        stale.dedupe_roots(&data);
        assert_eq!(names(&stale), vec!["Nitely", "AeraChromium"], "the last written entry wins");
        assert!(std::fs::read_to_string(kept("AeraChromium")).unwrap().contains("win-dev"));

        let mut alias = Config { repos: vec![repo("Old", "/dev/x"), repo("New", "/dev/x/")] };
        alias.dedupe_roots(&data);
        assert_eq!(std::fs::read_to_string(kept("New")).unwrap(), "old", "a dropped alias's playbook moves to the survivor");

        // When the copy fails (here the destination is a folder), nothing is dropped.
        std::fs::write(kept("Gone"), "rules").unwrap();
        std::fs::create_dir_all(kept("Blocked").with_extension("nitely-tmp")).unwrap();
        let mut failing = Config { repos: vec![repo("Gone", "/dev/y"), repo("Blocked", "/dev/y")] };
        failing.dedupe_roots(&data);
        assert_eq!(names(&failing), vec!["Gone", "Blocked"], "the old entry and its rules stay");
        assert!(failing.upsert_repo_in(&data, repo("Blocked", "/dev/y")).is_err());
        assert!(!kept("Blocked").exists());
        std::fs::remove_dir_all(data).ok();
    }
}
