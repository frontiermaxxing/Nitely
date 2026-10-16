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
7. Once the review passed (and Andrew approved, when it waits for him), call `land` (or
   `nitely land NIT-n`). It lands one branch at a time: rebase onto `main`, check the review still
   covers exactly this change, gate, fast-forward, push, clean up, and move the issue to Done. On a
   rebase conflict, resolve it in the worktree, then submit and land again.
8. Nitely is public. Code, UI text, CLI help, sample data, tests, comments, docs and commit messages
   never name another project Andrew works on, or its paths, branches, accounts or files: use
   made-up examples (`C:\Code\my-app`, "Acme", project key `APP`). An issue on the board may mention
   another repo as background; the code and docs describe the general case. Nitely's own repo, NIT
   project and paths are fine where they belong. User-facing text speaks to "you", never to Andrew.

## Build and test

- `cargo test --workspace` (core rules, MCP protocol), `cargo clippy --workspace`
- `npm test` (frontend tests: `src/**/*.test.ts`, compiled by tsc and run with node:test)
- `npm run build` (type-check and bundle the frontend)
- `npm run tauri dev` runs the app with hot reload
- `npm run dev` in a plain browser runs the frontend on sample data (`src/dev/fixtures.ts`); the
  Browser pane opens it with the `web` configuration in `.claude/launch.json`. Check every view you
  change there, in light and dark, wide and narrow. Other agents preview at the same time: use your
  own port, your own Browser pane tab (`tabs_create`), and scratch files and browser profiles in a
  folder named after your issue (docs/visualization.md, "The browser preview"). Add `?data=live` to check a view
  against the real database, read-only (`cargo build -p nitely-cli` first)
- The app icon is `src-tauri/icons/icon.svg` and, for 16 to 48px, `icon-small.svg`;
  `node scripts/app-icon.mjs` makes the sizes. Windows embeds the icon in the exe when it is built,
  so a new icon shows only after a rebuild (restart `npm run tauri dev`), and a pinned taskbar
  shortcut can keep the old one until Windows refreshes its icon cache.
- After landing a change to `crates/`, run `cargo run --release -p nitely-cli -- install` in the main
  checkout: it puts the new `nitely` where hooks and MCP run it, even while agents run the old one
  (`cargo install` fails then on Windows), and running MCP servers forward tool calls to it.

## Layout

- `crates/nitely-core`: database, issues and their rules, git reader, playbooks (`playbook.rs`),
  GitHub/Bitbucket/Linear sync, snapshot, `connect.rs` (adding a repo and wiring up its agents)
- `crates/nitely-cli`: the `nitely` binary, including `nitely mcp` (`src/mcp.rs`)
- `src-tauri`: the desktop shell and its commands; `src`: the React frontend, with the shared visual
  primitives in `src/ui` and design tokens in `src/styles/tokens.css`
- `docs/vision.md` is the end state and build order; `docs/design.md` is milestone 1; `docs/visualization.md` is
  the visual language every view follows; `docs/crew.md` is how Nitely runs agent workers itself
