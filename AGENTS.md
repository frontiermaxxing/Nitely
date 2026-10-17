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
- Land installs the new `nitely` itself after a change to `crates/`, `Cargo.toml` or `Cargo.lock`
  (a `[[land.after]]` step in `.nitely/playbook.toml`, run in the main checkout while land still
  holds its lock), so you do not. It puts the new build where hooks and MCP run it, even while
  agents run the old one, and running MCP servers forward tool calls to it. Only when submit or land
  refuses because the installed `nitely` is older than main (the land's install step failed: see
  its warning) run `cargo run --release -p nitely-cli -- install` in the main checkout yourself.

## Driving the real app

A live check (the title bar, notifications, window state, anything the browser preview cannot
show) is yours to run, in the real app on throwaway data, before it becomes a live-check issue.
**CI is where it runs.** `.github/workflows/app.yml` runs `npm run app:test` on Windows for every
pull request into `main`: push the branch (`submit` does) and read that job's result and its
screenshots. The harness opens a real window on the screen it runs on, and on the person's PC that
once came just before a hard freeze (NIT-188), so:

- `npm run app:test`, `npm run app:drive` and `toast.check.mjs` refuse to start outside CI unless
  `NITELY_HARNESS_LOCAL=1` is set for that run. Set it only when an issue really needs a local run,
  one run at a time, and never while the person is likely using the PC for something else.
- Even then the window never maximizes, restores or minimizes off CI (window.e2e.mjs leaves those
  steps to CI), and the harness app's WebView2 draws without the GPU (`--disable-gpu
  --disable-gpu-compositing` in `scripts/harness/tauri.harness.conf.json`; inbox.e2e.mjs checks it).
- `npm run app:build` (a few minutes; it builds into `target/harness`, apart from the dev build) is
  safe anywhere: it only compiles. The tests are `scripts/harness/*.e2e.mjs`; they need Node 22 (the
  driver uses its built-in WebSocket). One harness app runs at a time on a machine; a second launch,
  from any worktree, waits for it.
- `npm run app:drive` starts the app on seeded sample data and prints its DevTools port (WebView2 picks it
  and writes it to DevToolsActivePort) and data
  folder; drive it from a script with `scripts/harness/cdp.mjs` (`Page.connect(port)`: `open("#inbox")`,
  `click({ css, text })`, `waitFor(js)`, `screenshot(path)`, `invoke(command)`, `windowState()`).
  In a test, `launch()` from `scripts/harness/app.mjs` does the same with your own seed and
  `close()` cleans up; `restart(whileClosed)` closes it from its window and starts it again on the
  same data and storage; `notifications(home)` and `badge(home)` read the toasts the app asked to
  show and its taskbar count.
- It never touches the person's Nitely: its own identifier (so its own window, lock, storage and
  app data), a new temp data folder each run (the app refuses `NITELY_HARNESS=1` without one, and the
  real one always), no `nitely://` registration, no OS keychain at all (a throwaway approval key and
  paired-phone list in the process; its CLI calls, on the temp data folder, get keychain entries of
  their own, never the person's), and notifications written to `harness-notifications.jsonl` instead of shown (`NITELY_HARNESS_TOASTS=1`
  shows them). The window opens without taking the focus, but it is on the person's screen while
  a test runs (window.e2e.mjs maximizes it for a moment).
- One real toast, confirmed from Windows' notification history: `node scripts/harness/toast.check.mjs`
  (it shows a toast on the desktop, so it is not part of `app:test`).
- What a driven window cannot do: OS input outside the page (Alt+Space, Win+arrows, dragging the
  window, resizing by its border, clicking a toast). Do not send OS input on the person's desktop;
  leave those steps in the live-check issue, numbered, for them.

## Layout

- `crates/nitely-core`: database, issues and their rules, git reader, playbooks (`playbook.rs`),
  GitHub/Bitbucket/Linear sync, snapshot, `connect.rs` (adding a repo and wiring up its agents).
  A schema change is a new file in `crates/nitely-core/migrations/` (its README), never an edit of
  `SCHEMA` in `db.rs`
- `crates/nitely-cli`: the `nitely` binary, including `nitely mcp` (`src/mcp.rs`)
- `src-tauri`: the desktop shell and its commands; `src`: the React frontend, with the shared visual
  primitives in `src/ui` and design tokens in `src/styles/tokens.css`
- `docs/vision.md` is the end state and build order; `docs/design.md` is milestone 1; `docs/visualization.md` is
  the visual language every view follows; `docs/crew.md` is how Nitely runs agent workers itself
