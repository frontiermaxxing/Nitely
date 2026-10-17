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
not depend on the answer. Never set Approved: only the human approves. When a change needs their \
approval, `submit` already puts it in their inbox, so never `ask` for an approval.
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
    let gate = gate_line(&pb);
    // Read-only, and skipped when the database cannot be read: this runs at every session start.
    let offer = repo
        .project
        .as_deref()
        .filter(|p| crate::Db::open_read_only(&crate::paths::db_path()).and_then(|db| db.project_brief(p)).is_ok_and(|b| b.setup.is_none()))
        .map(|p| format!("\n\n{}", setup_offer(p)))
        .unwrap_or_default();
    // A light worktree says what it is and where the rest of the code is.
    let light = crate::playbook::current_worktree(dir)
        .ok()
        .and_then(|(top, _)| crate::light::session_note(&pb, std::path::Path::new(&repo.root), &top))
        .map(|n| format!("\n\n{n}"))
        .unwrap_or_default();
    Some(format!(
        "This repo ({}) is connected to Nitely.{project} Tasks branch from and land on {}. {gate}{offer}{light}\n\n{WORKFLOW}",
        repo.name,
        pb.integration_branch()
    ))
}

/// The gate an agent is told about at session start: its commands by name, so it can check them
/// (the setup sweep's pre-flight does) without reading the playbook file; a count when they are
/// too long to list.
fn gate_line(pb: &Playbook) -> String {
    let named: Vec<String> = pb
        .gate
        .commands
        .iter()
        .map(|c| format!("`{c}`"))
        .chain(pb.gate.steps.iter().map(|s| {
            let some = !s.when_changed.is_empty() || !s.unless_only.is_empty();
            format!("`{}`{}", s.run, if some { " (for some changes)" } else { "" })
        }))
        .collect();
    match named.len() {
        0 => "The playbook has no gate commands yet.".to_string(),
        n if named.iter().map(String::len).sum::<usize>() > 300 => format!("`submit` and `land` run its gate ({n} commands)."),
        _ => format!("`submit` and `land` run its gate: {}.", named.join(", ")),
    }
}

/// What a session in a project nobody has set up is told: offer the sweep, and without a yes run
/// only its cheap pre-flight, which catches a stale integration branch early.
pub fn setup_offer(project: &str) -> String {
    format!(
        "{project} has not been set up: no sweep has read this repo yet. Offer the user, in one line, to seed the {project} \
board from a sweep of the repo (the `setup_guide` tool; `/nitely-setup` in Claude Code), with its cost: one pass and a \
few minutes for a small repo, up to about 5 agents and 20 to 30 minutes for a large one. Start the sweep only on a \
yes, or when `next_task` handed you {project}'s setup issue. Without a yes, run only the guide's Phase 0 pre-flight \
(integration branch, gate commands, working tree) and file what it finds."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setup_offer_names_the_project_and_holds_back_the_full_sweep() {
        let offer = setup_offer("SHOP");
        assert!(offer.starts_with("SHOP has not been set up") && offer.contains("seed the SHOP board"), "{offer}");
        assert!(offer.contains("only on a yes") && offer.contains("Phase 0 pre-flight"), "{offer}");
        assert!(!offer.contains('\n'), "one paragraph");
    }

    #[test]
    fn the_gate_is_named_so_an_agent_can_check_it() {
        let none = Playbook::parse("").unwrap();
        assert_eq!(gate_line(&none), "The playbook has no gate commands yet.");
        let pb = Playbook::parse("[gate]\ncommands = [\"npm test\"]\n[[gate.steps]]\nrun = \"cargo test\"\nwhen_changed = [\"^crates/\"]\n").unwrap();
        assert_eq!(gate_line(&pb), "`submit` and `land` run its gate: `npm test`, `cargo test` (for some changes).");
        let long = Playbook::parse(&format!("[gate]\ncommands = [\"{}\", \"x\"]\n", "y".repeat(400))).unwrap();
        assert_eq!(gate_line(&long), "`submit` and `land` run its gate (2 commands).");
    }

    #[test]
    fn the_setup_guides_example_rules_are_a_valid_playbook() {
        // A Windows checkout has the file with CRLF line ends.
        let guide = SETUP_GUIDE.replace("\r\n", "\n");
        let start = guide.find("```toml\n").expect("the guide shows example rules") + "```toml\n".len();
        // The example is indented under a list item; TOML does not mind.
        let example = &guide[start..start + guide[start..].find("```").unwrap()];
        let pb = Playbook::parse(example).unwrap();
        assert!(pb.unknown.is_empty(), "every key is one Nitely reads: {:?}", pb.unknown);
        assert_eq!((pb.review.flags.len(), pb.approval.human_paths.len(), pb.approval.blocked_paths.len()), (1, 2, 1));
        for rule in pb.approval.human_paths.iter().chain(&pb.approval.blocked_paths) {
            regex::Regex::new(&rule.pattern).unwrap();
        }
    }
}
