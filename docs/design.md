# Nitely design

**Status:** draft, 2026-10-16. Nothing is scaffolded yet.

## Problem

Agents already work well with a disciplined workflow: one worktree and branch per issue, a PR step
with cross-vendor review, a land step with the build and test gate, Linear for the backlog, one
changelog file per change. What is missing is a view for the person running them. Sourcetree shows
branches and worktrees but not which agent owns them, what it is doing, whether review passed, or what
is waiting on you. Linear shows issues but not the git and PR state behind them. Answering "what is
happening right now, and what needs me" means stitching four tools together by hand.

Nitely is a small desktop app, plus a CLI and MCP server for agents, that joins those pieces into one
picture and later becomes the backlog itself, replacing Linear.

## Goals

1. **See, at a glance,** every active task: issue, agent, worktree, branch, last activity, PR, review
   verdict, land status.
2. **One inbox for what needs you:** approvals only a human gives (billing, release, sweeping copy),
   questions an agent is blocked on, owed live checks, failed lands and rebase conflicts.
3. **Agents report and read state locally** with no extra effort per task: Claude Code and Cursor first,
   Codex because it is the usual cross-vendor reviewer.
4. **Replace Linear** for your projects, keeping a strict issue vocabulary (kind, source, evidence).
5. **Lightweight and cross-platform:** Windows first, Mac next (with its own integration branch where a
   repo keeps one per OS).

Non-goals for now: running or launching agents, a cloud service or multi-user sync, replacing a repo's
existing workflow gates. Nitely observes and records; the existing scripts keep enforcing.

## Views

**Now (home).** One row per active task, grouped by repo. Columns: issue key and title, agent
(vendor and session), phase (started, committing, in review, changes asked, approved, landing, landed,
blocked), branch ahead/behind the integration branch, uncommitted file count, last commit, last agent
activity ("ran test:gate 2 min ago"), PR link, review verdict. A red or amber dot means it needs
you. Stale rows (no activity for a configurable time, worktree with no live session) sink and grey out.

**Needs me.** The inbox. Each item says what is asked and has the action in place: approve a change
that needs a human, answer an agent's question, mark a live check done, open a conflicted worktree.
This is the view the tray icon count points at.

**Task detail.** Timeline of everything that happened to one issue: created, worktree started, commits,
each review round with its findings (P0/P1/P2), PR opened or updated, land attempts with gate output
(new failures vs `known-failures.json`), changelog entry, handoff notes. Buttons to open the worktree in
the editor, a terminal, Sourcetree, or the PR in the browser.

**Branches.** The part of Sourcetree you actually use: the integration branch with task branches fanned
off it, each labeled with its issue and agent; worktrees with no branch, branches with no worktree, and
worktrees older than N days flagged (the "sprawl" that many agents in parallel leave behind).

**Land queue.** Who holds `.git/nitely-land.lock`, what is queued, and recent land results.

**Backlog.** Issues list with kind, source, component, severity and state filters; duplicate warnings
on create when an open issue shares 60% of the title's words. Until milestone 2 this is a read-only
mirror of Linear.

**Tray / menu bar.** Count of "needs me" items and a notification when one arrives or a land fails.

## Data model

All state lives in one SQLite file per user (`%APPDATA%\Nitely\nitely.db`, `~/Library/Application
Support/Nitely` on Mac) in WAL mode, so the app, the CLI and the MCP server can all read and write it,
and agents can report while the app is closed.

| Entity | Fields (main ones) | Notes |
|---|---|---|
| Repo | id, name, root path, worktrees dir, integration branch per OS, PR provider + remote | e.g. `dev-windows` / `dev-mac`, Bitbucket `team/my-app` |
| Issue | key (`APP-123`), title, body, kind, state, source, component, severity, evidence, created/updated | States and labels kept from Linear; `Approved` is human-only |
| Task | id, issue key, repo, branch, worktree path, vendor (`claude`/`codex`/`cursor`), created, closed | One per branch; an issue can have several over time |
| Session | id, vendor, external session id, task id, cwd, started, last heartbeat, state | A running agent; mapped to a task by its working directory |
| Event | id, ts, actor (agent session or human), task id, issue key, type, payload JSON | Append-only; everything on the timeline is an event |
| Review | task id, patch id, reviewer vendor, verdict, money/release, sweeping, findings JSON | One per review round, written by the PR step |
| PullRequest | provider, number, url, state, head sha, task id | Polled from Bitbucket (GitHub later) |
| Approval | issue key, patch id, by, ts | Written only from the app UI |
| Ask | id, task id, question, options, answer, answered ts | An agent's blocking question to you |

Live git state (worktree list, ahead/behind, dirty files, last commit) is not stored; the app reads it
from git and caches it in memory. Phase is derived: the latest event, combined with live git and PR
state, decides it, so a missed event cannot leave a row wrong forever.

**Attribution without effort.** Every agent hook or CLI call carries its working directory. Nitely
maps cwd → worktree → branch → issue key (branches are `claude/app-123-slug`), so an agent never has to
say which task it is on.

**Every call names its session.** Several agents often work from one folder, so an MCP call acts as
the session that made it, not as the folder (NIT-194): a Codex call names its thread
(`_meta.threadId`, `codex:<thread id>`), and a Claude Code call carries its tool use id, which the
session's PreToolUse hook noted with the session id (`claude:<session id>`, the id its other hooks
report). Without that hook, the session Claude Code names in `CLAUDE_CODE_SESSION_ID` stands in while
it is open, and else the folder (`claude:c:/code/app`), as every event before this did. Claims and
timeline entries carry that id; the folder stays the session's own (`place` on a timeline entry).
The app shows a session as its vendor and the end of its id, `Claude · 3317`, and the full id on
hover. A crew worker keeps its folder's id, which is its own worktree.

**Approvals are a guardrail, not a security boundary.** The CLI and MCP server refuse to set `Approved`
or write an Approval. An agent with shell access could
still write the SQLite file directly. If that matters later, the app can sign approvals with a key held
in the OS keychain and `land` can verify the signature.

## How agents integrate

Three layers, each useful on its own.

**1. Passive hooks (no agent cooperation needed).** The live "what is it doing" column comes from hooks
that call `nitely hook <vendor>` with the hook's JSON on stdin.

- Claude Code: `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Stop`, `SubagentStop` in
  `.claude/settings.json`, next to any PreToolUse hooks the repo already has, and `PreToolUse` for MCP
  tools only (`mcp__.*`), which notes the session behind each call and nothing else. The hook records a
  heartbeat and a one-line summary (tool name, command or file), never full prompts or file contents.
- Cursor: its `hooks.json` (shell execution, file edit, stop events) maps the same way.
- Codex: the `notify` program in `config.toml` gives turn-complete events.

The exact event names for Cursor and Codex are to be checked against their current docs when we build
it; the design only needs "session started, did something, stopped" from each.

**2. Explicit reporting: CLI and MCP.** One binary, `nitely`, with JSON output:

- `nitely status "<what I'm doing>"`, `nitely note --handoff "<text>"`, `nitely ask "<question>"
  [--option ...]` (creates a Needs-me item; the agent can poll `nitely ask --wait <id>`)
- `nitely issue find|create|move|show` with the backlog's rules (kind, source, evidence;
  Done needs `--evidence`, Canceled/Duplicate need `--reason`)
- `nitely event <type> --json <payload>` for scripts
- `nitely mcp` runs a stdio MCP server exposing the same operations as tools (`get_my_task`,
  `set_status`, `add_note`, `ask_human`, `find_issues`, `create_issue`, `move_issue`). Registered in
  `.mcp.json` for Claude Code and `.cursor/mcp.json` for Cursor; Codex reads MCP servers from its config.

**3. Existing workflow scripts.** A repo's own `start`, `pr` and `land` scripts call `nitely event` at
each step (worktree created, review verdict with findings, PR opened, gate result, landed or failed).
That is a few lines per command and gives the timeline its backbone. In milestone 2, a script's Linear
client gets a Nitely sibling with the same interface, and a config switch picks the backend, so moving
off Linear changes no agent instructions.

Agents keep following `AGENTS.md`; Nitely adds one paragraph there ("report status with `nitely
status`; ask the human with `nitely ask`") rather than a new workflow.

## Tech stack

**Tauri 2, with a Rust core and a React + TypeScript + Vite frontend, and SQLite through `rusqlite`.**

- **Why Tauri:** it uses the system webview (WebView2 on Windows, WKWebView on Mac), so the installer is
  around 10 MB and idle memory is a fraction of Electron's, which matters for an app that sits in the
  tray all day. It builds for Windows, Mac and Linux from one codebase, has a tray API, and the Rust
  side can ship the same code as a standalone CLI.
- **Why one Rust workspace:** crates `nitely-core` (database, git reader, PR providers, issue rules),
  `nitely-app` (Tauri), and `nitely-cli` (the `nitely` binary, including `nitely mcp`). The CLI starts in
  milliseconds, which matters for hooks that run on every tool call, and does not need Node.
- **Git:** shell out to the user's `git` (`worktree list --porcelain`, `rev-list --count`, `status
  --porcelain`) so behaviour matches their config (sparse worktrees, `core.autocrlf`). A file
  watcher on `.git/worktrees` and `refs/` triggers refreshes instead of polling.
- **PRs and Linear:** Bitbucket and Linear REST/GraphQL from Rust with `reqwest`, reading tokens from
  `credentials.json` in Nitely's data folder (`%APPDATA%\Nitely`), later the OS keychain.
- **Why React:** familiar and well-supported for list, table and graph views. Solid or Svelte would be
  lighter, but the frontend is not where the weight is in a Tauri app.

Alternatives considered: Electron (heavier, but would let us reuse Node workflow scripts directly); a local web
app served by the CLI (lightest, but no tray, notifications or "open in editor" without extra work);
native per platform (best feel, double the work).

## Milestones

**M1: see everything (read-only).** The Sourcetree replacement for one repo.

- Tauri app on Windows with Now, Task detail, Branches and Needs-me views
- Git reader for the repo and its worktrees; Bitbucket PR poller; Linear issues read through the API
- `nitely hook claude` wired into Claude Code hooks for live activity
- Needs-me populated from what can be inferred: issues in Needs You, PRs waiting on a human
  approval, failed lands, rebases in progress, owed live checks
- Done when you can answer "what is each agent doing and what is waiting on me" without opening
  Sourcetree or Linear

**M2: agents report, Nitely records.** CLI and MCP server (`status`, `note`, `ask`), workflow scripts emit
events, review findings on the timeline, Cursor hooks.

**M3: Nitely is the backlog.** Native issues, one-time Linear import that keeps issue keys and history,
a Nitely backend for workflow scripts, approvals given in the app and checked by `land`.

**M4:** Mac build and per-OS integration branches, Codex notify integration, GitHub as a second PR provider, more repos.

## Open questions

1. Keep an imported project's Linear prefix after leaving Linear, or a per-repo prefix?
2. Should the Claude hook record only tool names and commands (proposed) or also prompt text?
