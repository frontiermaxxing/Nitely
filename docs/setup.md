# Setting up a repo

How `nitely connect` (or **+ Add project** in the app) adds a repo, wires up your agents and seeds the board, and what the hooks record. Back to the [README](../README.md).

## Add a project

In the app, choose **+ Add project** and pick a folder you already work in (for example
`C:\Code\my-app`). Nitely shows what it will do before it does anything:

- creates the project (key and name editable) and watches the repo and its worktrees;
- picks a playbook: the repo's own `.nitely/playbook.toml` if it has one, otherwise one generated
  from the repo (its default branch, `cargo test`, npm scripts). A generated playbook is kept in
  Nitely's data folder, not in the repo;
- connects every agent it finds, at user level, so nothing is written into the repo:
  - Claude Code: hooks in `~/.claude/settings.json` (existing hooks kept), the `nitely` and
    `nitely-setup` skills in `~/.claude/skills`, and `claude mcp add --scope user nitely -- nitely mcp`;
  - Codex: `[mcp_servers.nitely]` and `notify` in `~/.codex/config.toml` (comments kept), and the
    workflow between `<!-- nitely:start -->` markers in `~/.codex/AGENTS.md`;
  - Cursor: `~/.cursor/mcp.json` and `~/.cursor/hooks.json`.

Every agent session that starts in a connected repo gets that repo's instructions (project key,
integration branch, gate, workflow) from the session-start hook and the MCP server. Running it again
changes only what is out of date. The CLI does the same:

```powershell
nitely connect C:\Code\my-app                 # show the plan
nitely connect C:\Code\my-app --project APP --yes
```

The skill, Codex instructions and hook and MCP registrations live in your home folder, so they go
stale when a newer Nitely writes them differently. The app then shows a banner to review and update
them; `nitely agents` shows the same, and `nitely agents --yes` updates them.

**Seed the board.** A new project's board starts empty. Ask an agent in the repo to set it up
(`/nitely-setup` in Claude Code; any agent can follow the `setup_guide` MCP tool). It sweeps the
repo read-only, checks the integration branch and the gate first, files what it finds (human-only
work goes to your Needs You), proposes review rules for you to approve in the Inbox, and writes the
project's brief (the project page's Brief view). Nothing is committed or run against live systems.

Manual setup still works (`nitely project create`, `nitely repo add`, `nitely issue import`).
Approving and answering asks happen only in the app, never through the CLI or MCP.

## Faster builds on Windows (optional)

Nothing here is needed: Nitely works without it, and it never changes these settings itself. It is
for when several agents build at once and the PC feels slow.

Windows Defender scans every file a build writes, and a Rust or npm build writes thousands. With a
few builds at once, Defender can take more CPU than the builds do (four cores, on one PC with seven
agents). [Build lanes](build-lanes.md) already cut what gets built (agents edit in light worktrees,
and a few warm lanes per project do the building), so this matters less than it did. Two things help
further, and both are yours to decide:

- **Exclude the folder your builds happen in** from real-time scanning. Builds then run at full
  speed; the trade-off is that Defender no longer scans files as they are written there, so only
  exclude folders that hold your own repos' worktrees and build output. With the default playbook,
  worktrees and lanes are both under the folder beside your repo (`C:\Code\my-app-worktrees` for
  `C:\Code\my-app`). From an administrator PowerShell:

  ```powershell
  Add-MpPreference -ExclusionPath "C:\Code\my-app-worktrees"
  ```

  or **Windows Security > Virus & threat protection > Manage settings > Exclusions**. Your own
  checkout's `target` folder can be excluded the same way. `Remove-MpPreference -ExclusionPath ...`
  undoes it.
- **A Dev Drive** (Windows 11): a ReFS volume that Defender scans asynchronously, so builds are not
  held up. Make one in **Settings >
  System > Storage > Advanced storage settings > Disks & volumes > Create Dev Drive**
  ([Microsoft's guide](https://learn.microsoft.com/windows/dev-drive/)), then point the playbook's
  `[worktree] dir` at it. It needs free disk space to give it and admin rights to create.

**sccache** caches compiled crates across checkouts. It does not help a warm lane: the lane already
keeps its build, and the workspace's own crates, which every change recompiles, are not cached. On
this repo it could save at most 42 of a cold lane's 66 s (dependencies compiled, workspace not),
which a lane pays once, so Nitely does not set it up. If you use it anyway, `RUSTC_WRAPPER=sccache`
in your environment reaches lanes too, since they run with Nitely's environment.

This repo's own debug profile is lighter for the same reason (`[profile.dev]` in `Cargo.toml`: line
numbers for its own code, no debuginfo for dependencies), which halves a lane's `target/`. A Rust
repo of yours can do the same.


## Hooks

`nitely connect` installs them. The hooks (`nitely hook claude|codex|cursor`) record tool names,
commands and file paths, never prompt text or file contents, and always exit 0, so they can't block
an agent. [docs/claude-hooks.json](claude-hooks.json) is the Claude Code part, for setting it
up by hand. Its PreToolUse hook runs before MCP calls and shell commands: it notes which session
makes each MCP call, so the board can say which agent holds an issue (NIT-194), and it reminds an
agent about to build or test in a light worktree to use `run` instead (the playbook's `[lanes]
heavy`; NIT-202). The command still runs. When a Nitely version adds a hook, Agent
setup in the app (or `nitely agents`) shows the agent's settings as a change to make; applying it
covers every repo, since the hooks are set up once per user.

Agents can also report directly: `nitely status "running test:gate"` from inside a worktree.
