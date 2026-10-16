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

## Track a project in Nitely

```powershell
nitely project create NIT Nitely
nitely repo add C:\Development\Nitely --integration main --project NIT
nitely issue import docs\seed-backlog.json --project NIT
```

`.mcp.json` (Claude Code) and `.cursor/mcp.json` (Cursor) register `nitely mcp` for this repo; the
agent picks the project from the repo it runs in. Approving and answering asks happen only in the
app, never through the CLI or MCP.

## Bitbucket and Linear

Nitely reads `%USERPROFILE%\.aera-work\credentials.json`, the same file `scripts/work/credentials.mjs`
uses, and never writes it. The app syncs every 60 seconds and on "Sync now"; `nitely sync` does the
same from a terminal and prints what it got. Both are read-only on Bitbucket and Linear. Pull
requests come from each watched repo's Bitbucket `origin` plus the repo named in the credentials
file; issues are the open ones in the Linear project the file names.

## Hook up Claude Code

Merge [docs/claude-hooks.json](docs/claude-hooks.json) into your user settings
(`%USERPROFILE%\.claude\settings.json`) so every repo reports, or into one repo's
`.claude/settings.json` next to its existing hooks. The hook records tool names, commands and file
paths, never prompt text or file contents, and always exits 0, so it can't block an agent.

Agents can also report directly: `nitely status "running test:gate"` from inside a worktree.
