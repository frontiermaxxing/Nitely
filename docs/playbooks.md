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
event goes on the issue's timeline, so flakes are counted rather than hidden. A `[lanes]` table gives
the repo build lanes: warm checkouts where `nitely run` (MCP: `run`) builds and tests a snapshot of a
worktree instead of the worktree itself ([build-lanes.md](build-lanes.md)). Without it nothing
changes. [docs/playbooks/example.toml](playbooks/example.toml) is a worked example of a full
playbook for a made-up monorepo: per-OS branches, worktree setup and teardown, gate steps that skip
what a change cannot affect, review flags, and paths that wait for you.

## Submodules that land with the repo

A repo whose code lives partly in a git submodule (an engine, a vendored fork with your own commits on
it) can have a task change both, and land both together. Declare the submodule:

```toml
[[submodules]]
path = "engine"     # the submodule's folder in this repo
branch = "main"     # the submodule's integration branch; the repo's own name when left out
remote = "origin"   # where the submodule's branch is pushed; left out, it stays on this machine
sparse = ["core", "ui/widgets"]  # cone-mode directories to check out; left out, the whole submodule
checkout = "when_needed"         # or "every_task": when a task gets the submodule's worktree
```

Leave `remote` out for a submodule with no remote of its own (its only remote is an upstream you
never push to): its integration branch then lives only in the repo's own copy of the submodule.

- **start_task** cuts the task branch in the submodule too, as a worktree of the repo's own copy of
  the submodule at `<worktree>/engine`, from the commit the repo points it at. Commit there, then
  commit the moved pointer in the task worktree (`git add engine`), like any other change. With
  `checkout = "when_needed"` (the default) it does so only when the task needs it: when the issue
  names a path in it (`engine/core/net.c`), when the task already has work there, or on the first
  `widen` of a path under it (`nitely widen engine/ui/widgets`, MCP: `widen`). A task that never
  touches the submodule never writes it, its folder left empty as an unchecked-out submodule's is.
  `"every_task"` makes it for every task (when the repo's tests read the submodule, say). With
  `sparse`, the worktree is a cone-mode sparse checkout of those directories, plus those of the
  paths the issue names in it; `widen` adds more. A large submodule then costs a task only the
  part it works in.
- **submit** refuses a submodule with uncommitted changes or commits the pointer does not name (the
  reviewer would not see them), and the reviewer reads the submodule's code in the diff
  (`git diff --submodule=diff`), not a one-line pointer change. The review's patch covers the
  submodule's code, so a rebase that leaves it the same keeps the review; a pointer whose code git
  cannot show counts by its commit ids. With `remote`, the submodule's task branch is pushed with
  the repo's, so the pull request's pointer resolves.
- **land** rebases the submodule's branch onto its integration branch, rebases the task branch onto
  the repo's, points it at the rebased submodule commit, runs the gate, and then fast-forwards the
  submodule's integration branch and the repo's. All or nothing: if the second fast-forward fails,
  the first is undone, and neither branch moves. The repo's own copy of the submodule follows: a
  checkout that has the branch moves with it, and one detached at the old tip (as
  `git submodule update` leaves it) moves to the new one. With `remote`, every declared submodule's
  branch is pushed before the repo's, on every land, and a failed submodule push keeps the repo's
  push back, so the pushed repo never points at a commit its remote cannot fetch. A submodule the
  change leaves alone is moved in the task's worktree to where the rebased branch points it, so the
  gate builds what lands. A repo with declared submodules lands each change alone, never in a batch.
- **gate steps** can run only when the submodule moved (`when_changed = ["^engine$"]`). In a build
  lane the submodule is checked out at the commit the snapshot points at, sparse over its `sparse`
  directories plus those the snapshot changes in it since the base, and `[lanes] keep` entries
  under its folder (`"engine/out"`) keep its build output between requests. Large-repo mode builds
  in your checkout and leaves its submodules alone, so it refuses a snapshot that points one
  elsewhere.

Each commit of the task branch keeps the pointer it was made with; the last one points at the
rebased submodule commit. Uncommitted submodule changes are not part of a `run` snapshot: commit
them in the submodule first.


## Private terms

If you work on public code next to private work, list what must never reach a project's repos
(another project's name, a client, a local folder) in the project's **Settings** in the app, or for
every project at once. `submit` and `land` refuse a branch that adds one to a line, a new file path
or a commit message in any of its commits, and say only where (`src/app.ts:12 (commit 1a2b3c4):
term 2 of APP's list`). The list stays in Nitely's data folder on this machine; no MCP tool or CLI
command shows it, so it never reaches an agent's context.
