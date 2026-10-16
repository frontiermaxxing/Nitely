# Working on Nitely

Nitely tracks its own build. Its backlog is the **NIT** project in Nitely, reached through the
`nitely` MCP server registered in `.mcp.json` (Cursor: `.cursor/mcp.json`).

## How to work

1. Take work with `next_task`. It claims the most urgent Todo issue for you. Then call `start_task`
   with its key: it creates the issue's branch and worktree from `main` and runs `npm ci` there, as
   `.nitely/playbook.toml` says. Work only in that worktree, never on `main`.
2. Use `report` for what you are doing at each step; it shows live on Andrew's board.
3. Anything you find but will not fix now goes in with `file_issue` (kind, body, evidence). It refuses
   likely duplicates and lists them.
4. When only Andrew can decide, use `ask`, move the issue to Needs You, and keep working on whatever
   does not depend on the answer. Never set Approved; only Andrew can.
5. Before you stop, leave a `note` with `handoff: true`: what is done, what is left, what you were
   unsure about.
6. When the work is committed, call `submit` (or `nitely submit`). It runs the gate, has the other
   vendor review the branch (Codex reviews Claude, Claude reviews the rest), pushes, opens or
   updates the pull request into `main`, and moves the issue: back to In Progress when the review
   finds P0 or P1 problems (fix them and submit again), to Needs You when Andrew must approve, and
   to In Review otherwise. `run_gate` alone runs just the gate.
7. Move the issue to Done with evidence once the pull request is merged.

## Build and test

- `cargo test --workspace` (core rules, MCP protocol), `cargo clippy --workspace`
- `npm run build` (type-check and bundle the frontend)
- `npm run tauri dev` runs the app with hot reload
- `cargo install --path crates/nitely-cli` refreshes the `nitely` binary that hooks and MCP use

## Layout

- `crates/nitely-core`: database, issues and their rules, git reader, playbooks (`playbook.rs`),
  GitHub/Bitbucket/Linear sync, snapshot
- `crates/nitely-cli`: the `nitely` binary, including `nitely mcp` (`src/mcp.rs`)
- `src-tauri`: the desktop shell and its commands; `src`: the React frontend
- `docs/vision.md` is the end state and build order; `docs/design.md` is milestone 1
