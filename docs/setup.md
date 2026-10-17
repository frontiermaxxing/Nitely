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

## Builds in many worktrees

Every task gets its own worktree, so a repo with several agents at work has several checkouts
building at once. For a Rust repo, Nitely points all of a repo's task worktrees at one shared cargo
build directory: it writes a `.cargo/config.toml` in the folder that holds the worktrees (for
`dir = "../{repo}-worktrees/{key}-{slug}"`, that is `my-app-worktrees`), which every build under it
reads, the agents' own `cargo` runs included. Dependencies then compile once instead of once per
worktree; on Nitely itself a new worktree's first `cargo test` went from 111 s and 5,500 files
written to 47 s and about 1,500. Land clears the shared directory when it passes
`shared_build_max_gb`, and deletes the `target/` of a worktree it has to keep because a process
still uses it. `shared_build = false` under `[worktree]` turns it off. Nitely writes the file only
where the folder holds nothing but worktrees, and never over a `.cargo/config.toml` of your own.

### Windows: Defender and a Dev Drive

Windows Defender scans every file as it is written, and builds write tens of thousands. With a
few agents building at once, Defender can take more CPU than the builds. Windows 11's answer is a
**Dev Drive**: a ReFS volume that Defender scans asynchronously, in its performance mode, so builds
are not held up. Make one in **Settings > System > Storage > Advanced storage settings > Disks &
volumes > Create Dev Drive** ([Microsoft's guide](https://learn.microsoft.com/windows/dev-drive/)),
then put the worktrees on it in the playbook:

```toml
[worktree]
dir = "D:/my-app-worktrees/{key}-{slug}"
```

`nitely connect` and the app's Agent setup page say when a repo's worktrees are on a volume
Defender scans in real time, and name a Dev Drive you already have. Nitely never changes Defender's
settings; whether to use a Dev Drive, or to exclude a folder from scanning, is yours to decide.


## Hooks

`nitely connect` installs them. The hooks (`nitely hook claude|codex|cursor`) record tool names,
commands and file paths, never prompt text or file contents, and always exit 0, so they can't block
an agent. [docs/claude-hooks.json](claude-hooks.json) is the Claude Code part, for setting it
up by hand.

Agents can also report directly: `nitely status "running test:gate"` from inside a worktree.
