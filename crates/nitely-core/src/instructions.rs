//! What an agent is told about working through Nitely. One text, delivered three ways so every
//! agent gets it without a file in the repo: the MCP server's `instructions`, the Claude Code
//! SessionStart hook's output, and the user-level skill and AGENTS.md block that `connect` writes.

use crate::config::Config;
use crate::playbook::Playbook;

/// The workflow every connected agent follows, independent of the repo.
pub const WORKFLOW: &str = "\
Nitely tracks this work. Use the `nitely` MCP tools; do not track work anywhere else. `get_project` \
returns the project's brief, what earlier agents learned about it: read it before exploring the repo, \
and when you learn something about the project as a whole that the next agent needs, add it with \
`set_project_brief`.

1. `next_task` claims the most urgent Todo issue. Then `start_task` with its key creates the issue's \
branch and worktree from the integration branch and runs setup. Work only in that worktree.
2. `report` one line on what you are doing at each step; it shows live on the human's board.
3. `file_issue` anything you find but will not fix now (it refuses likely duplicates); `file_issues` \
files many at once, their blockers naming each other by ref. When the playbook itself is wrong \
(integration branch, gate, review rules), `propose_playbook_change`: never change the rules yourself.
4. When only the human can decide, `ask`, move the issue to Needs You, and keep working on what does \
not depend on the answer. Never set Approved: only the human approves.
5. Commit, then `submit`: it runs the gate, has another vendor review the branch, opens the pull \
request, and moves the issue. Read its `next`.
6. Once it is In Review (or the human approved it), `land` puts it on the integration branch.
7. Before you stop, `note` with handoff: true: what is done, what is left, what you were unsure about.
8. Link every issue you name, in replies, commit messages, pull requests and notes: \
[NIT-14](nitely://issue/NIT-14), the `url` the tools return. It opens the issue in the Nitely app.

Priority when you file: 1 exploitable today, data loss or a leak, a production job silently stopped, \
or a broken release; 2 a user-visible bug, a blocker for shipping, or a missing safety net (CI, \
environment switching); 3 correctness or debt that matters but can wait; 4 cleanup and nice-to-haves. \
State: Todo when an agent can do it now; Backlog for ideas, priority 4 and work waiting on another \
issue (or Todo with blocked_by naming it); owner human for work only the human can do; product \
decisions go through `ask`.";

/// The first-run sweep that seeds a new project's board from its repo: the `nitely-setup` skill
/// `connect` installs for Claude Code, and what the `setup_guide` tool returns to every agent.
pub const SETUP_GUIDE: &str = include_str!("setup_guide.md");

/// The skill's description: when an agent should reach for it.
pub const SETUP_DESCRIPTION: &str = "Set up a project in Nitely: sweep the repo once and seed its board with issues, a project brief and proposed review rules. Use it when asked to set up or onboard a project, or when get_project shows setup is null.";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setup_guides_example_rules_are_a_valid_playbook() {
        let start = SETUP_GUIDE.find("```toml\n").expect("the guide shows example rules") + "```toml\n".len();
        // The example is indented under a list item; TOML does not mind.
        let example = &SETUP_GUIDE[start..start + SETUP_GUIDE[start..].find("```").unwrap()];
        let pb = Playbook::parse(example).unwrap();
        assert!(pb.unknown.is_empty(), "every key is one Nitely reads: {:?}", pb.unknown);
        assert_eq!((pb.review.flags.len(), pb.approval.human_paths.len(), pb.approval.blocked_paths.len()), (1, 2, 1));
        for rule in pb.approval.human_paths.iter().chain(&pb.approval.blocked_paths) {
            regex::Regex::new(&rule.pattern).unwrap();
        }
    }
}
