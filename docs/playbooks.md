# Playbooks

How a task becomes a branch, a gate, a review and a land, and how to keep private names out of a repo. Back to the [README](../README.md).

## Playbooks

A repo's `.nitely/playbook.toml` says how a task becomes work: the branch it is cut from, the
branch and worktree names, the commands that set up a new worktree, and the gate that must pass
before a pull request. `nitely start NIT-3 --agent claude` (MCP: `start_task`) creates the branch
and worktree and runs setup; `nitely gate` (MCP: `run_gate`) runs the gate in the current worktree.
`nitely submit` (MCP: `submit`) hands in a committed branch: it runs the gate, has another vendor
review it with a fixed closing format (`VERDICT`, plus the playbook's yes/no flags), decides whether
a human must approve (`[approval]` paths, or a flag the reviewer raised), pushes, and opens or updates
the pull request on GitHub or Bitbucket. The review is stored for that exact patch, so landing can
check it. `nitely land NIT-4` (MCP: `land`) is then the only writer of the integration branch:
one land at a time under a lock, rebase, a review that must cover exactly the rebased change, a
human approval in Nitely when the change needs one, the gate, a fast-forward and a push. All of it
shows on the issue's timeline and in the app. A repo without a playbook gets `main`, `{vendor}/{key}-{slug}`
branches and worktrees in `../{repo}-worktrees/`. Nitely's own is in [.nitely/playbook.toml](../.nitely/playbook.toml).
Gate steps can depend on what the branch changes (`when_changed`, `unless_only`), commands can use
`{main}`, `{worktree}` and `{base}`, and `[approval] blocked_paths` lists paths no task branch may
touch. A gate command that fails runs once more at the same commit before the gate fails
(`[gate] retries`, 1 by default, 0 to turn it off). One that then passes is a flake: the gate
passes, its result lists the command and the tests that failed under `flaked`, and a `gate_flaked`
event goes on the issue's timeline, so flakes are counted rather than hidden. [docs/playbooks/example.toml](playbooks/example.toml) is a worked example of a full
playbook for a made-up monorepo: per-OS branches, worktree setup and teardown, gate steps that skip
what a change cannot affect, review flags, and paths that wait for you.


## Private terms

If you work on public code next to private work, list what must never reach a project's repos
(another project's name, a client, a local folder) in the project's **Settings** in the app, or for
every project at once. `submit` and `land` refuse a branch that adds one to a line, a new file path
or a commit message in any of its commits, and say only where (`src/app.ts:12 (commit 1a2b3c4):
term 2 of APP's list`). The list stays in Nitely's data folder on this machine; no MCP tool or CLI
command shows it, so it never reaches an agent's context.
