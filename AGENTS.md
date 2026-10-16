# Working on Nitely

Nitely tracks its own build. Its backlog is the **NIT** project in Nitely, reached through the
`nitely` MCP server registered in `.mcp.json` (Cursor: `.cursor/mcp.json`).

## How to work

1. Take work with `next_task`. It claims the most urgent Todo issue for you and suggests a branch
   name. Work on that branch, not on `main`.
2. Use `report` for what you are doing at each step; it shows live on Andrew's board.
3. Anything you find but will not fix now goes in with `file_issue` (kind, body, evidence). It refuses
   likely duplicates and lists them.
4. When only Andrew can decide, use `ask`, move the issue to Needs You, and keep working on whatever
   does not depend on the answer. Never set Approved; only Andrew can.
5. Before you stop, leave a `note` with `handoff: true`: what is done, what is left, what you were
   unsure about.
6. Open a pull request into `main` from the branch `next_task` named; Nitely links it to the issue
   by that name and shows its checks. Move the issue to In Review, and to Done with evidence once
   it is merged.

## Build and test

- `cargo test --workspace` (core rules, MCP protocol), `cargo clippy --workspace`
- `npm run build` (type-check and bundle the frontend)
- `npm run tauri dev` runs the app with hot reload
- `cargo install --path crates/nitely-cli` refreshes the `nitely` binary that hooks and MCP use

## Layout

- `crates/nitely-core`: database, issues and their rules, git reader, Bitbucket/Linear sync, snapshot
- `crates/nitely-cli`: the `nitely` binary, including `nitely mcp` (`src/mcp.rs`)
- `src-tauri`: the desktop shell and its commands; `src`: the React frontend
- `docs/vision.md` is the end state and build order; `docs/design.md` is milestone 1
