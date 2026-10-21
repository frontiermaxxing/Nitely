//! What core needs of the crew (docs/crew.md): the playbook's `[crew]` rules, and the reads of the
//! crew's tables that the board, the land and the session list make. The supervisor itself is in
//! nitely-crew (NIT-497).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::db::Db;

/// Workers a crew may start per project per local day when the playbook does not say.
pub const DEFAULT_MAX_PER_DAY: u32 = 20;

/// The playbook's `[crew]` section. `workers = 0`, the default, is off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrewRules {
    /// How many workers run at once.
    pub workers: u32,
    /// Agents in preference order; the ones not installed are skipped.
    pub agents: Vec<String>,
    /// Workers started per local day; `DEFAULT_MAX_PER_DAY` when unset.
    pub max_per_day: Option<u32>,
    /// Tokens the project's agents may use per local day before no new worker starts (crew
    /// workers and everyone else, as `usage` reads them); unset, the default, is no budget.
    pub max_tokens_per_day: Option<u64>,
    /// `HH:MM-HH:MM` local time when no new worker starts; it may wrap midnight.
    pub quiet_hours: Option<String>,
    /// A worker running longer is stopped, never while it lands.
    pub timeout_minutes: u64,
    /// A worktree setup running longer is stopped, and its issue goes to a person.
    pub setup_timeout_minutes: u64,
    /// Failed workers on one issue before it goes to a human.
    pub attempts: u32,
    /// Files nearly every change touches (registries, shared types): holding one counts for a
    /// quarter when `next_task` keeps new work out of files in flight (overlap.rs).
    pub shared_files: Vec<String>,
    /// A worker with no output in its log and no report for this long is stopped as hung, and its
    /// issue handed on; 0 is off.
    pub idle_minutes: u64,
    /// No new worker starts while less than this is free where worktrees go (GB); 0 is off. Memory
    /// is the machine's own admission (NIT-266, machine.rs).
    pub min_free_disk_gb: u64,
    /// How many workers may work on one goal at once (a goal's own `workers` wins); 0, the
    /// default, is no cap below `workers`.
    pub goal_workers: u32,
    /// Replaces the default command for an agent. `{worktree}`, `{repo}`, `{git_dir}`,
    /// `{mcp_config}`, `{nitely}` and `{nitely_env}` are filled in; the brief goes on stdin.
    pub commands: BTreeMap<String, Vec<String>>,
}

impl Default for CrewRules {
    fn default() -> Self {
        CrewRules {
            workers: 0,
            agents: vec!["claude".into(), "codex".into()],
            max_per_day: None,
            max_tokens_per_day: None,
            quiet_hours: None,
            timeout_minutes: 180,
            setup_timeout_minutes: 30,
            attempts: 3,
            shared_files: vec![],
            idle_minutes: 30,
            min_free_disk_gb: 10,
            goal_workers: 0,
            commands: BTreeMap::new(),
        }
    }
}

impl CrewRules {
    pub fn max_per_day(&self) -> u32 {
        self.max_per_day.unwrap_or(DEFAULT_MAX_PER_DAY)
    }

    /// The command that starts `agent`: the playbook's, else the default, else None.
    pub fn command_for(&self, agent: &str) -> Option<Vec<String>> {
        if let Some(c) = self.commands.get(agent).filter(|c| !c.is_empty()) {
            return Some(c.clone());
        }
        let args = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect());
        match agent {
            // auto: edits, builds and commits go ahead without a person; prompts none: anything that
            // would still ask is refused instead of waiting for an answer nobody gives.
            "claude" => args(&[
                "claude", "-p", "--mcp-config", "{mcp_config}", "--permission-mode", "auto", "--permission-prompts", "none",
                "--output-format", "stream-json", "--verbose",
            ]),
            // Without default_tools_approval_mode, exec refuses every MCP call under approval_policy
            // never. No sandbox: workspace-write never lets git write under a `.git` folder (not with
            // --add-dir, not with writable_roots), so a worker could not commit in its worktree; this
            // is the trust Claude's auto mode gets. Seen in the live check of NIT-118.
            "codex" => args(&[
                "codex", "exec", "--sandbox", "danger-full-access",
                "-c", "approval_policy=\"never\"",
                "-c", "mcp_servers.nitely.command={nitely}",
                "-c", "mcp_servers.nitely.args=[\"mcp\"]",
                // Codex starts MCP servers with a cleared environment: without this, a crew on
                // its own data folder (NITELY_HOME) would have workers on the real one.
                "-c", "mcp_servers.nitely.env={nitely_env}",
                "-c", "mcp_servers.nitely.default_tools_approval_mode=\"approve\"",
                "-",
            ]),
            _ => None,
        }
    }
}

impl Db {
    /// Marks the sessions a crew worker runs: in its worktree, started once the worker had. A
    /// database without the crew tables (an older one, read-only) marks none.
    pub fn mark_crew(&self, sessions: &mut [crate::db::Session]) {
        let Some(oldest) = sessions.iter().map(|s| s.started_at).min() else { return };
        let workers: Vec<(String, i64, Option<i64>, String)> = (|| -> rusqlite::Result<_> {
            let mut stmt = self.conn.prepare("SELECT worktree, started_at, ended_at, agent_id FROM crew_worker WHERE ended_at IS NULL OR ended_at >= ?1")?;
            let rows = stmt.query_map([oldest - 60], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect()
        })()
        .unwrap_or_default();
        if workers.is_empty() {
            return;
        }
        let workers: Vec<(String, i64, Option<i64>, String)> = workers.into_iter().map(|(w, s, e, a)| (crate::paths::normalize(&w), s, e, a)).collect();
        for s in sessions.iter_mut() {
            let cwd = crate::paths::normalize(&s.cwd);
            s.crew = workers.iter().any(|(wt, started, ended, agent_id)| {
                let inside = cwd == *wt || cwd.strip_prefix(wt.as_str()).is_some_and(|rest| rest.starts_with('/'));
                // A minute's slack: the worker's row is written just before its process starts.
                let began_in_it = s.started_at >= started - 60 && ended.is_none_or(|e| s.started_at <= e);
                // A worker's reports go to a session named after its agent id, which an earlier
                // session in the same worktree may already have started: it counts while the worker
                // is there.
                let reports_in_it = s.id == *agent_id && s.last_seen_at >= started - 60 && ended.is_none_or(|e| s.last_seen_at <= e + 60);
                inside && (began_in_it || reports_in_it)
            });
        }
    }

    /// Whether a crew worker is running in `worktree` now.
    pub fn crew_worker_in(&self, worktree: &Path) -> Result<bool> {
        let mut stmt = self.conn.prepare("SELECT worktree FROM crew_worker WHERE ended_at IS NULL")?;
        let rows: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let wanted = crate::paths::normalize(&worktree.to_string_lossy());
        Ok(rows.iter().any(|w| crate::paths::normalize(w) == wanted))
    }
}
