//! What an agent is told about working through Nitely. One text, delivered three ways so every
//! agent gets it without a file in the repo: the MCP server's `instructions`, the Claude Code
//! SessionStart hook's output, and the user-level skill and AGENTS.md block that `connect` writes.

use crate::config::Config;
use crate::playbook::Playbook;

/// The workflow every connected agent follows, independent of the repo.
pub const WORKFLOW: &str = "\
Nitely tracks this work. Use the `nitely` MCP tools; do not track work anywhere else.

1. `next_task` claims the most urgent Todo issue. Then `start_task` with its key creates the issue's \
branch and worktree from the integration branch and runs setup. Work only in that worktree.
2. `report` one line on what you are doing at each step; it shows live on the human's board.
3. `file_issue` anything you find but will not fix now (it refuses likely duplicates).
4. When only the human can decide, `ask`, move the issue to Needs You, and keep working on what does \
not depend on the answer. Never set Approved: only the human approves.
5. Commit, then `submit`: it runs the gate, has another vendor review the branch, opens the pull \
request, and moves the issue. Read its `next`.
6. Once it is In Review (or the human approved it), `land` puts it on the integration branch.
7. Before you stop, `note` with handoff: true: what is done, what is left, what you were unsure about.";

/// The instructions for an agent working in `dir`: the workflow plus what Nitely knows about the
/// repo, or None when `dir` is not in a repo Nitely watches.
pub fn for_dir(config: &Config, dir: &str) -> Option<String> {
    let repo = config.repo_for_dir(dir)?;
    let pb = Playbook::load(std::path::Path::new(&repo.root)).unwrap_or_default();
    let project = repo.project.as_deref().map(|p| format!(" Its issues are in project {p} ({p}-n keys).")).unwrap_or_default();
    let gate = pb.gate.commands.len() + pb.gate.steps.len();
    let gate = match gate {
        0 => "The playbook has no gate commands yet.".to_string(),
        n => format!("`submit` and `land` run its gate ({n} commands)."),
    };
    Some(format!(
        "This repo ({}) is connected to Nitely.{project} Tasks branch from and land on {}. {gate}\n\n{WORKFLOW}",
        repo.name,
        pb.integration_branch()
    ))
}
