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
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = paths::config_path();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::config_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    /// Adds a repo, or replaces the one with the same name.
    pub fn upsert_repo(&mut self, repo: RepoConfig) {
        self.repos.retain(|r| r.name != repo.name);
        self.repos.push(repo);
    }
}
