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


## Hooks

`nitely connect` installs them. The hooks (`nitely hook claude|codex|cursor`) record tool names,
commands and file paths, never prompt text or file contents, and always exit 0, so they can't block
an agent. [docs/claude-hooks.json](claude-hooks.json) is the Claude Code part, for setting it
up by hand; its PreToolUse hook runs before MCP calls only and notes which session makes each one,
so the board can say which agent holds an issue (NIT-194). When a Nitely version adds a hook, Agent
setup in the app (or `nitely agents`) shows the agent's settings as a change to make; applying it
covers every repo, since the hooks are set up once per user.

Agents can also report directly: `nitely status "running test:gate"` from inside a worktree.
