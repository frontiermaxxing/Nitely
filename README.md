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
`update_issue` (Done needs evidence; only a human approves), `ask` and `get_answer`, and
`get_project` / `set_project_brief` / `mark_setup_complete` for the project's brief (what agents
learned about it, editable on the project's Brief view) and whether its setup sweep has run.
`propose_playbook_change` files a change to the repo's playbook for the human to apply or reject in
the Inbox; agents never change the rules themselves.

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
nitely connect C:\Code\my-app --yes      # a repo you work in
npm run tauri dev
```

`cargo test` runs the core tests. `nitely now` prints the same board as JSON, and `nitely where`
prints where the database and config live (`%APPDATA%\Nitely`).

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
touch. [docs/playbooks/example.toml](docs/playbooks/example.toml) is a worked example of a full
playbook for a made-up monorepo: per-OS branches, worktree setup and teardown, gate steps that skip
what a change cannot affect, review flags, and paths that wait for you.

## Private terms

If you work on public code next to private work, list what must never reach a project's repos
(another project's name, a client, a local folder) in the project's **Settings** in the app, or for
every project at once. `submit` and `land` refuse a branch that adds one to a line, a new file path
or a commit message in any of its commits, and say only where (`src/app.ts:12 (commit 1a2b3c4):
term 2 of APP's list`). The list stays in Nitely's data folder on this machine; no MCP tool or CLI
command shows it, so it never reaches an agent's context.

## GitHub

Watched repos whose `origin` is on GitHub get their pull requests, approvals and CI checks synced,
with no setup beyond a signed-in GitHub CLI (`gh auth login`). Nitely uses `NITELY_GITHUB_TOKEN`
or `GITHUB_TOKEN` instead when set. A PR shows on the issue its branch names (`claude/nit-7-...`).

## Bitbucket and Linear

Nitely reads `credentials.json` in its data folder (`%APPDATA%\Nitely` on Windows; `nitely where`
prints it) and never writes it:

```json
{
  "bitbucket": { "email": "you@example.com", "apiToken": "…", "workspace": "acme", "repo": "my-app" },
  "linear": { "apiKey": "lin_api_…", "projectId": "…" }
}
```

Either section can be left out. Nitely reads no other location: if you kept this file somewhere
else before, move it here (or point `NITELY_CREDENTIALS` at it).

The app syncs every 60 seconds and on "Sync now"; `nitely sync` does the same from a terminal and prints what it got. Both are read-only on Bitbucket and Linear. Pull
requests come from each watched repo's Bitbucket `origin` plus the repo named in the credentials
file; issues are the open ones in the Linear project the file names. To leave Linear, `nitely
import-linear --project APP` moves a Linear project into native issues once, keeping its keys; it
shows what it would do until you add `--yes`, and `--state "Needs Review=Needs You"` chooses the
Nitely state for a Linear state Nitely has no name for (an open state named "Needs …" is Needs You
unless you choose otherwise). To keep such a choice for the Linear mirror
too, put it in `config.json` in the data folder: `"linear_states": { "Needs Review": "Needs You" }`.

## Agents on other machines on your network

The backlog lives in the database on the machine that runs Nitely. `nitely serve` lets agents on
other machines on the same local network (a Mac beside a Windows PC, say) reach it: it is the
`nitely` MCP server over HTTP at `/mcp`, so Claude Code there needs no Nitely install.

1. In the app, open **Other machines** and add one (say `mac`, default project APP). It shows that
   machine's token once, inside the command to run there, with a Copy button.
2. Run `nitely serve` on this machine. It listens on port 7421 and announces itself on the network.
3. On the other machine, run the copied command:

```bash
claude mcp add --transport http nitely "http://192.168.1.68:7421/mcp" --header "Authorization: Bearer nly_..."
```

`nitely discover` (or `dns-sd -B _nitely._tcp` on a Mac without Nitely) finds this machine's address.

- **Local network only.** Requests from addresses outside the machine's own networks (private
  ranges, link-local, or the same subnet as one of its interfaces) are refused, so a forwarded port
  never exposes the backlog. It speaks plain HTTP: keep it to a network you trust.
- **A token per machine, made only in the app.** It is shown once, to you; no command an agent can
  run prints or stores a token, and Nitely keeps only its hash. `nitely tokens list` shows names and
  projects, and removing a machine (in the app, or `nitely tokens remove mac`) cuts it off. Its
  agents act as `claude@mac`, and the token's project is the default for their calls.
- **The backlog, not the repo.** Remote agents get `next_task`, `list_issues`, `get_issue`,
  `file_issue`, `update_issue`, `report`, `note`, `ask`, `get_answer` and the project brief tools. `start_task`, `run_gate`,
  `submit` and `land` work in a repo on the machine that runs them, so they are not offered.
  Approving stays in the app, as everywhere.
- **Firewall.** The first time `nitely serve` listens, Windows may ask whether to allow it on private
  networks; allow it there, and only there.
- `nitely discover` checks each announced address before listing it, so what it prints answers.
- Remote agents show on Now under "On other machines", and count among the agents working on Pulse.
- The server is small on purpose: one thread per connection (32 at most, 4 per address), one
  10-second deadline per request however slowly it arrives, a
  Content-Length body of at most 1 MB, and the connection closed after each answer.

## Hooks

`nitely connect` installs them. The hooks (`nitely hook claude|codex|cursor`) record tool names,
commands and file paths, never prompt text or file contents, and always exit 0, so they can't block
an agent. [docs/claude-hooks.json](docs/claude-hooks.json) is the Claude Code part, for setting it
up by hand.

Agents can also report directly: `nitely status "running test:gate"` from inside a worktree.

## Contributing

Issues and pull requests are welcome; [CONTRIBUTING.md](CONTRIBUTING.md) says how. Commits need a
`Signed-off-by` line (`git commit -s`, the Developer Certificate of Origin).

## License

Copyright 2026 Quixet LLC. Licensed under the [Apache License 2.0](LICENSE); see [NOTICE](NOTICE).
A Nitely build includes open-source packages under their own licenses, listed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
