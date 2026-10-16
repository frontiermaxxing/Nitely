use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

use crate::paths;

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
        config.dedupe_roots();
        Ok(config)
    }

    /// Keeps one entry per repo root, the last written. Older versions replaced repos by name only,
    /// so adding a watched repo under another name left a stale entry that lookups by root found first.
    fn dedupe_roots(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut kept: Vec<RepoConfig> = self.repos.drain(..).rev().filter(|r| seen.insert(paths::normalize(&r.root))).collect();
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
    pub fn upsert_repo(&mut self, repo: RepoConfig) {
        let root = paths::normalize(&repo.root);
        self.repos.retain(|r| r.name != repo.name && paths::normalize(&r.root) != root);
        self.repos.push(repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, root: &str) -> RepoConfig {
        RepoConfig { name: name.into(), root: root.into(), integration_branch: "main".into(), project: None }
    }

    #[test]
    fn one_entry_per_root() {
        let mut config = Config { repos: vec![repo("Aera", r"C:\Dev\AeraChromium"), repo("Nitely", "/dev/Nitely")] };
        config.upsert_repo(repo("AeraChromium", "C:/Dev/AeraChromium/"));
        let names: Vec<&str> = config.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Nitely", "AeraChromium"]);

        let mut stale = Config { repos: vec![repo("Aera", r"C:\Dev\AeraChromium"), repo("Nitely", "/dev/Nitely"), repo("AeraChromium", "C:/Dev/AeraChromium")] };
        stale.dedupe_roots();
        let names: Vec<&str> = stale.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Nitely", "AeraChromium"], "the last written entry wins");
    }
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
