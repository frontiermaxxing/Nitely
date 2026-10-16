# Nitely

A desktop app for running a team of coding agents: a backlog they take work from, a live view of
what each one is doing, and an inbox for the calls only you should make. Where it is going:
[docs/vision.md](docs/vision.md). Milestone 1 design: [docs/design.md](docs/design.md). How agents
work on this repo: [AGENTS.md](AGENTS.md).

**Views:** Inbox (agents' questions with an answer box, issues waiting on you with Approve, stuck
worktrees), Now (every worktree with its agent, PR and git state), and one backlog per project with
an issue panel showing the full timeline.

**For agents:** `nitely mcp` is an MCP server with `next_task` (atomic claim of the most urgent Todo
issue), `report`, `note` (with handoffs), `file_issue` (duplicate check, evidence required),
`update_issue` (Done needs evidence; only a human approves), `ask` and `get_answer`.

## Layout

| Path | What |
|---|---|
| `crates/nitely-core` | Config, SQLite store, git reader, hook parsing, the Now snapshot |
| `crates/nitely-cli` | The `nitely` binary agents and hooks call |
| `src-tauri` | The Tauri 2 desktop shell |
| `src` | React + TypeScript frontend (Vite) |

## Prerequisites (Windows)

Rust (MSVC toolchain, via rustup), Node 20+, Visual Studio Build Tools with "Desktop development
with C++", and WebView2 (already on Windows 11).

## Run it

```powershell
npm install
cargo install --path crates/nitely-cli     # puts nitely.exe on PATH (~/.cargo/bin)
nitely repo add C:\Development\AeraChromium --integration win-dev --name Aera
npm run tauri dev
```

`cargo test` runs the core tests. `nitely now` prints the same board as JSON, and `nitely where`
prints where the database and config live (`%APPDATA%\Nitely`).

## Add a project

In the app, choose **+ Add project** and pick a folder you already work in (for example
`C:\Development\AeraChromium`). Nitely shows what it will do before it does anything:

- creates the project (key and name editable) and watches the repo and its worktrees;
- picks a playbook: the repo's own `.nitely/playbook.toml` if it has one, Aera's when it finds
  `scripts/work/work.mjs`, otherwise one generated from the repo (its default branch, `cargo test`,
  npm scripts). Generated and Aera playbooks are kept in Nitely's data folder, not in the repo;
- connects every agent it finds, at user level, so nothing is written into the repo:
  - Claude Code: hooks in `~/.claude/settings.json` (existing hooks kept), the `nitely` skill in
    `~/.claude/skills/nitely`, and `claude mcp add --scope user nitely -- nitely mcp`;
  - Codex: `[mcp_servers.nitely]` and `notify` in `~/.codex/config.toml` (comments kept), and the
    workflow between `<!-- nitely:start -->` markers in `~/.codex/AGENTS.md`;
  - Cursor: `~/.cursor/mcp.json` and `~/.cursor/hooks.json`.

Every agent session that starts in a connected repo gets that repo's instructions (project key,
integration branch, gate, workflow) from the session-start hook and the MCP server. Running it again
changes only what is out of date. The CLI does the same:

```powershell
nitely connect C:\Development\AeraChromium              # show the plan
nitely connect C:\Development\AeraChromium --project AC --yes
```

Manual setup still works (`nitely project create`, `nitely repo add`, `nitely issue import`).
Approving and answering asks happen only in the app, never through the CLI or MCP.

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
branches and worktrees in `../{repo}-worktrees/`. Nitely's own is in [.nitely/playbook.toml](.nitely/playbook.toml).
Gate steps can depend on what the branch changes (`when_changed`, `unless_only`), commands can use
`{main}`, `{worktree}` and `{base}`, and `[approval] blocked_paths` lists paths no task branch may
touch. [docs/playbooks/aera.toml](docs/playbooks/aera.toml) is Aera's `scripts/work` workflow as a
playbook; copy it into the Aera repo as `.nitely/playbook.toml` to run Aera through Nitely.

## GitHub

Watched repos whose `origin` is on GitHub get their pull requests, approvals and CI checks synced,
with no setup beyond a signed-in GitHub CLI (`gh auth login`). Nitely uses `NITELY_GITHUB_TOKEN`
or `GITHUB_TOKEN` instead when set. A PR shows on the issue its branch names (`claude/nit-7-...`).

## Bitbucket and Linear

Nitely reads `%USERPROFILE%\.aera-work\credentials.json`, the same file `scripts/work/credentials.mjs`
uses, and never writes it. The app syncs every 60 seconds and on "Sync now"; `nitely sync` does the
same from a terminal and prints what it got. Both are read-only on Bitbucket and Linear. Pull
requests come from each watched repo's Bitbucket `origin` plus the repo named in the credentials
file; issues are the open ones in the Linear project the file names.

## Hooks

`nitely connect` installs them. The hooks (`nitely hook claude|codex|cursor`) record tool names,
commands and file paths, never prompt text or file contents, and always exit 0, so they can't block
an agent. [docs/claude-hooks.json](docs/claude-hooks.json) is the Claude Code part, for setting it
up by hand.

Agents can also report directly: `nitely status "running test:gate"` from inside a worktree.
