# Working on Nitely

Nitely tracks its own build. Its backlog is the **NIT** project in Nitely, reached through the
`nitely` MCP server registered in `.mcp.json` (Cursor: `.cursor/mcp.json`).

## How to work

1. Take work with `next_task`. It claims the most urgent Todo issue for you. Then call `start_task`
   with its key: it creates the issue's branch and a light worktree from `main` (source only: no
   `npm ci`, no build output, about 7 MB; `.nitely/playbook.toml`, `[worktree] mode = "light"`).
   Work only in that worktree, never on `main`. To read code as it is on `main`, use the reference
   tree `start_task` names (`../Nitely-worktrees/.nitely-lanes/Nitely/ref`): read-only, never edit
   or build there. A worktree made before NIT went light stays full until it lands. `start_task`
   also claims the issue for you, for an issue you were given by name too, and refuses one another
   agent holds while it is still working on it.
2. Use `report` for what you are doing at each step; it shows live on Andrew's board.
3. Anything you find but will not fix now goes in with `file_issue` (kind, body, evidence). It refuses
   likely duplicates and lists them.
4. When only Andrew can decide, use `ask` with the issue's key, and keep working on whatever does not
   depend on the answer. `ask` moves the issue to Needs You, and his answer moves it back to you (to
   Todo if you have stopped) and reaches you as a notice on your next nitely call. Never set
   Approved; only Andrew can. A question about the board or
   the code (who holds a file, whether an issue is done, whether a finding is stale, resubmit or
   wait) goes to `triage`: a triage agent answers it from the board and the repo, as a note on the
   issue, and only one that turns out to be Andrew's reaches his Inbox.
5. Before you stop, leave a `note` with `handoff: true`: what is done, what is left, what you were
   unsure about.
6. When the work is committed, call `submit` (or `nitely submit`). It hands the branch in and
   returns at once; in the background it runs the gate in a build lane (below), has the other vendor
   review the branch
   (Codex reviews Claude, Claude reviews the rest), pushes, opens or updates the pull request into
   `main`, and moves the issue: back to In Progress when the review finds P0 or P1 problems (fix
   them and submit again), to Needs You when Andrew must approve, and to In Review otherwise. The
   result comes back through `next_task`, `submit_status` (`nitely submit-status`), or a notice on
   your next nitely call. `submit` with `wait: true` (`nitely submit --wait`) blocks for the result
   as before. `run_gate` alone runs just the gate.
7. Once the review passed (and Andrew approved, when it waits for him), call `land` (or
   `nitely land NIT-n`). It lands one branch at a time: rebase onto `main`, check the review still
   covers exactly this change, gate (in a lane, ahead of everything else queued), fast-forward,
   push, clean up, and move the issue to Done. On a rebase conflict, resolve it in the worktree,
   then land again: a resolution that only combines the two sides keeps the review, and anything
   more is reviewed at the land, with no new submit.
8. Nitely is public. Code, UI text, CLI help, sample data, tests, comments, docs and commit messages
   never name another project Andrew works on, or its paths, branches, accounts or files: use
   made-up examples (`C:\Code\my-app`, "Acme", project key `APP`). An issue on the board may mention
   another repo as background; the code and docs describe the general case. Nitely's own repo, NIT
   project and paths are fine where they belong. User-facing text speaks to "you", never to Andrew.

## Build and test

Builds and tests run in **build lanes** (docs/build-lanes.md): two warm checkouts of this repo that
keep `target/` and `node_modules/` between requests, beside the worktrees in
`.nitely-lanes/Nitely/lane-1` and `lane-2`. Send a command there with `run` (MCP:
`run { command: "cargo test -p nitely-core" }`) or `nitely run -- cargo test -p nitely-core` from
your worktree: it builds a snapshot of the worktree as it is, committed or not, and returns the exit
code, the end of the output and the log (`run_status` waits on a long one; `nitely lanes` shows the
lanes and the queue). A warm lane compiles a nitely-core change in seconds; your worktree would
build everything from nothing, about 6 GB of it. So `cargo build`, `test`, `clippy`, `check`,
`npm test` and `npm run build` in your worktree still run, but a hook reminds you to use `run`
instead. The gate's commands, all through `run`:

- `cargo test --workspace` (core rules, MCP protocol), `cargo clippy --workspace -- -D warnings`
- `npm test` (frontend tests: `src/**/*.test.ts`, compiled by tsc and run with node:test)
- `npm run build` (type-check and bundle the frontend)

What still runs in your worktree, so install for it only when you need it:

- `npm ci` once before the browser preview or the app: `node_modules` is not in a light worktree.
- `npm run tauri dev` runs the app with hot reload (a full local build; rarely needed for a change
  the preview can show)
- `npm run dev` in a plain browser runs the frontend on sample data (`src/dev/fixtures.ts`); the
  Browser pane opens it with the `web` configuration in `.claude/launch.json`. Check every view you
  change there, in light and dark, wide and narrow. Other agents preview at the same time: use your
  own port, your own Browser pane tab (`tabs_create`), and scratch files and browser profiles in a
  folder named after your issue (docs/visualization.md, "The browser preview"). Add `?data=live` to check a view
  against the real database, read-only: it asks the installed `nitely`, or this worktree's
  `target/debug/nitely` when you built one (`cargo build -p nitely-cli`, only when the view needs a
  read command your branch adds)
- The app icon is `src-tauri/icons/icon.svg` and, for 16 to 48px, `icon-small.svg`;
  `node scripts/app-icon.mjs` makes the sizes. Windows embeds the icon in the exe when it is built,
  so a new icon shows only after a rebuild (restart `npm run tauri dev`), and a pinned taskbar
  shortcut can keep the old one until Windows refreshes its icon cache.
- Land installs the new `nitely` itself after a change to `crates/`, `Cargo.toml` or `Cargo.lock`
  (a `[[land.after]]` step in `.nitely/playbook.toml`, run in the background once the land lock is
  free, once for a burst of lands), so you do not, and you never wait for it: submit and land go
  ahead with an installed `nitely` that is behind main and say so. It puts the new build where hooks
  and MCP run it, even while agents run the old one, and running MCP servers forward tool calls to
  it. Only when submit or land refuses because the installed `nitely` cannot read the database (a
  newer build upgraded it) run `cargo run --release -p nitely-cli -- install` in the main checkout.
- `nitely lanes slim` reclaims the build output of task worktrees whose work is finished (it asks
  first, and keeps any worktree with uncommitted or unpushed work or recent activity).
- A land that cannot remove its worktree (your shell or dev server still in it) says so on the
  issue, and the next land removes it once nothing uses it (likewise any closed issue's clean,
  merged worktree). `nitely lanes leftovers` lists them with why each stays; `--remove` removes
  the ones that go now.

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
  --disable-gpu-compositing` in `scripts/harness/tauri.harness.conf.json`; every harness launch checks it).
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
- It never touches the person's Nitely or their agents: its own identifier (so its own window, lock,
  storage and app data), a new temp data folder each run and an agents' home inside it
  (`NITELY_AGENT_HOME`, so Agent setup and the token counts never see the real `~/.claude`, `~/.codex`
  or `~/.cursor`; the app refuses `NITELY_HARNESS=1` without either, and the real ones always), no
  `nitely://` registration, no OS keychain at all (a throwaway approval key and paired-phone list in
  the process; its CLI calls, on the temp data folder, get keychain entries of their own, never the
  person's), no updates, and notifications written to `harness-notifications.jsonl` instead of shown
  (`NITELY_HARNESS_TOASTS=1` shows them). Anything new that reaches outside the app's folders (a
  scheduled task, a login item, the registry) fails `everything_the_app_reaches_outside_its_folders_is_listed`
  in src-tauri/src/harness.rs until it is listed there with how the harness stays clear of it.
  The window opens without taking the focus, but it is on the screen while a test runs.
- `scenario.e2e.mjs` runs one whole flow across features (NIT-253) on a throwaway repo it connects:
  setup, a crew worker, async submit, review alone and batched, gates in a build lane, the land
  queue landing two changes on one gate, a finding the agent asks to have accepted, its
  notification and the signed Accept in the Inbox, checking the board, tracks and timelines at
  each step. Its agents and reviewer are fakes (`fake-agent.mjs`, the crew's command;
  `fake-reviewer.mjs`, the playbook's reviewer) acting through `nitely mcp` (`mcp.mjs`), and the
  app starts the crew's supervisor with the harness's own `nitely`. Add a step there when a new
  feature crosses others.
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
- `crates/nitely-crew`: the crew (`crew.rs`) and what is built on the core without the core
  needing it: token usage, the leaderboard, waits, triage, the pulse, performance reads, reports.
  Nothing in nitely-core may depend on it, so a change there skips nitely-core's tests. Its
  methods on `Db` are extension traits (`CrewDb`, `TriageDb`, `UsageDb`): import the trait to call
  one
- `crates/nitely-cli`: the `nitely` binary, including `nitely mcp` (`src/mcp.rs`)
- `src-tauri`: the desktop shell and its commands; `src`: the React frontend, with the shared visual
  primitives in `src/ui` and design tokens in `src/styles/tokens.css`
- `server/leaderboard`: the Leaderboard service (its README). The person hosts it; agents build and
  test it against a local instance only, and never deploy it
- `docs/vision.md` is the end state and build order; `docs/design.md` is milestone 1; `docs/visualization.md` is
  the visual language every view follows; `docs/crew.md` is how Nitely runs agent workers itself;
  `docs/build-lanes.md` is the design, with measurements, for many agents per machine (light
  worktrees, builds in shared warm lanes); `docs/coordinator.md` is what a coordinator chat did and where each of
  its jobs lives in Nitely (goal 4: Nitely runs the team)
