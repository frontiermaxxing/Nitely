# Visualization: how Nitely shows work

**Status:** set by NIT-24, 2026-10-16; redrawn by NIT-61 to Andrew's "Nitely Redesign" (Claude
Design, 2026-10-16). Every view builds on this: the Overview, the issue track, the decision Inbox,
activity lanes, the board and the branch map. If a view needs something this file does not cover,
extend the file in the same change.

Andrew runs many agents across several projects. Nitely is useful only if he can tell, in a few
seconds, **what needs him, what is moving and what is stuck**. Showing that clearly is the
product.

![The Overview, dark](img/overview-dark.png)

![Now with an issue open, light](img/now-and-track-light.png)

![The visual kit, dark](img/kit-dark.png)

## Principles

1. **Answer at a glance.** The top of every screen says whether anything needs Andrew, what is
   moving and what is stuck, before any detail. One line is enough: see the header of Now
   (`5 moving  2 waiting on you  4 idle sessions`) and the summary on a project page.
2. **Shape before text.** State is shown by position, color and form: where a token sits on the
   track, which shape a mark has, whether a dot breathes. Text is there to confirm, on hover
   (`title`) or in the issue panel. If you need a sentence to say what state something is in,
   draw it instead.
3. **One vocabulary everywhere.** An issue is always an `IssueToken`. An agent is always an
   `AgentMark`. Progress is always the lifecycle track. A view never invents its own badge for
   these.
4. **One colour rule.** Position says where an issue is; colour says who has it: **teal** is an
   agent moving it, **amber** is waiting on you, **red** has failed, **grey** is at rest (queued or
   done). The glyph, the mini track, the board columns, the flow boxes and the swimlanes all follow
   it. Amber is used only for what needs the human. Quiet things fade: done, canceled and stale go
   to `--fg-3` or lower opacity. Live work moves a little (the breathing dot); nothing else
   animates.
5. **Simple.** Few colors, no decoration, no chart junk, no pills around every label, no glows or
   drop shadows on cards. Light and dark are both first-class. Linear is the bar for polish.

## The lifecycle track

Every issue moves along one track with fixed stations:

```
Todo → Started → Working → Gate → Review → Needs you → Landing → Done
                    ↖──── changes asked ────╯
         any station ─→ stuck
```

| Station   | Reached when (timeline event, or the state)               | Whose move |
| --------- | --------------------------------------------------------- | ---------- |
| Todo      | `issue_created`; state Todo or Backlog (Backlog is dashed) | queued     |
| Started   | `claimed` or `worktree`                                    | agent      |
| Working   | a `status`, `note` or `handoff` after starting             | agent      |
| Gate      | `gate_passed` or `gate_failed`                             | agent      |
| Review    | `review_started`; state In Review                          | agent      |
| Needs you | state Needs You (only when the playbook wants a human)     | human      |
| Landing   | state Approved, `land_failed` or `landed`                  | agent      |
| Done      | `landed`; state Done                                       | done       |

Branches off the track:

- **Changes asked:** a `submit` note saying the review requested changes (or a `changes` review
  record). Drawn as a dashed loop from Review back over to Working, labelled with the number of
  rounds. Each further round adds a thinner solid arc around it (up to five), so an issue that
  went round four times reads as a coil before you read the label. The mini track draws a small
  arc over the same stretch.
- **Stuck:** the last gate failed, the last land stopped, the review did not finish, or the
  submit could not push the branch, and nothing has passed since (a later submit or a move
  clears a stopped review; a failed push holds until the next submit or a person moves it). The current
  station turns `--st-stuck` and the line onward breaks into dashes. A stuck worktree that waits
  on the human (a rebase conflict, an agent waiting for input) is the attention color instead,
  because it needs him.
- **Asked you:** an open `ask`. An attention dot on the current station (and on the glyph).
- **Closed:** Canceled or Duplicate. The whole track fades and the station reads Canceled or
  Duplicate.

Stations before the current one that were never reached are **skipped** (small hollow ring); most
issues skip Needs you. All of this is computed in one place, the backend's
`crates/nitely-core/src/track.rs` (with its tests). It puts a `track` on every issue the app
receives (`issues`, `issue_detail`, `snapshot`, `pulse`): open issues from their timelines, closed
issues in lists from their state. On the frontend always call `trackOf(issue)` from
`src/ui/lifecycle.ts`, which returns that `track`; its `computeTrack` mirrors the Rust rules only for
the browser preview's sample data. A rule change goes into `track.rs` first, then `computeTrack`,
with a case in `crates/nitely-core/src/track_cases.json`: both `cargo test` and `npm test` run every
case there, so the two cannot drift apart.
Never compute stations anywhere else. `track.since` is when the issue last arrived at its station
(known only from a timeline): the board's "age in this column".

## Primitives (`src/ui/`)

Import from `./ui` (or `../ui`). Open the browser preview at `#kit` to see each one in every state.

| Primitive      | What it shows                                                                 | Use it for |
| -------------- | ----------------------------------------------------------------------------- | ---------- |
| `IssueToken`   | Station glyph + key, optionally title and kind. Opens the panel (native) or Linear. | Any mention of an issue. |
| `StationGlyph` | Where the issue stands, as one of seven 14px forms (below).                   | Dense rows where the key sits in its own column. |
| `StationTrack` | `size="full"`: every station, labels, times, loop, stuck. `size="mini"`: the eight stations as dots on a line, the current one large and colored. | Full: the issue sheet, a focused decision. Mini: rows that mix states. |
| `AgentMark`    | A rounded square in the vendor's color with its letter, and a notched live dot. | Any mention of an agent or the human. |
| `StateDot`     | A session's liveness on its own: working (breathes), waiting, idle, stale, ended. | Legends, glance lines, places without a vendor. |
| `Sparkline`    | A line or bars, oldest first.                                                 | Activity over time next to a number. |
| `Meter`        | Parts of a whole as one segmented bar.                                       | Open issues by phase, checks by result. |
| `CountBadge`   | A count: `attention` (filled), `live`, or `quiet`.                           | Nav items and section headings. |
| `EmptyState`   | An empty track, one sentence, what would fill it.                            | Every list or page that can be empty. |
| `Loading`      | One quiet line, shown only after 300ms so fast reads never flash.            | Every view or panel before its first data. |
| `PriorityMark` | Issue priority as three rising bars; urgent is red.                          | Issue rows. |
| `PrMark`       | A pull request's number and its checks; merged and declined fade.            | Cards and rows that mention a PR (`PrCell` in src/Pr.tsx has the long form). |
| `ChecksMark`   | CI checks: a tick, a red cross, a dashed ring while they run. `label` adds words. | Next to a PR. |
| `AheadBehind`  | Commits behind the base (faint, left) and ahead of it (right) around a tick. | Branches. Pass the same `max` to rows shown together. |
| `SeverityMark` | A review finding's P0 to P3. Only P0 and P1 are red.                         | Review findings. |
| `EventMark`    | Something that happened to an issue, as a 12px shape: ring started, dot commit, square gate, diamond review, target land, bar handoff, attention dot ask. Red only for failures. | Marks on a timeline (activity lanes), legends. |
| `FindingDots`  | A review's findings, one small square each, most severe first; carried over is hollow. | A review round or verdict in a row. |
| `ResultMark`   | A run's result: quiet tick, red cross, or dashed ring while it runs.         | Gate runs, lands, CI checks. |

The glyph's forms (`glyphKindOf`): a grey ring is Todo (dashed in Backlog); a teal ring with a dot
is an agent's station (Started to Review); an amber ring with a halo is Needs you; solid teal with
a hole is Landing; solid grey is Done; a red ring with a dot is stuck; a struck-through ring is
canceled or a duplicate. An open ask adds a small amber dot at the top right.

Helpers: `livenessOf(session, now)` (adds "stale" after 30 minutes quiet), `vendorOf(actor)`,
`phaseOf(station)`, `stationColor(station, stuck)`, `describeTrack(track)` for hover text,
`journeyOf(timeline, reviews)` (`src/ui/rounds.ts`) for the rounds between Working and Done
(each round's gate runs, its review and verdict, then each land), and `describeFindings`.

### Agents

| Vendor | Shape                  | Token              |
| ------ | ---------------------- | ------------------ |
| Claude | rounded square with C  | `--vendor-claude`  |
| Codex  | rounded square with X  | `--vendor-codex`   |
| Cursor | rounded square with U  | `--vendor-cursor`  |
| Human  | ring                   | `--vendor-human`   |
| Other  | rounded square with ?  | `--vendor-other`   |

Vendor colors are muted on purpose, so a screen full of agents stays calm and none of them
competes with the attention color. The letter carries identity as much as the color does. Live
state is the small dot at the bottom right: breathing `--live` when working, `--attention` when
waiting for the human, `--fg-3` when idle; stale marks fade to 45%, ended to 30%.

## The issue sheet

![An issue after five review rounds, dark](img/issue-rounds-dark.png)

An issue opens as a sheet over the view (`src/IssuePanel.tsx`), its story on the left and the
decision and facts on a rail on the right. The rail starts with "Waiting on you" (why, Approve,
Back to Todo) when the issue is in Needs You, then the facts (state, agent, reviewer, branch, pull
request, kind, last gate), "Waiting on" and "Blocks", the body and the moves.

On the left, top to bottom: the key and title, the full `StationTrack` on a card, **Who had it**,
then the issue's journey. Who had it (`WhoHadIt` in `src/IssueJourney.tsx`) is a lane per owner
(the agent that wrote it, the gate, the reviewer, you) with a cell per step in time order, so a
hand-off reads as a step down and a round of changes as work coming back up; back-to-back gate
runs share one cell ("✕ ✓ Pass"). The journey (`src/IssueJourney.tsx`): one row per round with its
gate runs (`ResultMark`s) and its review
(verdict and `FindingDots`), then the pull request with its checks, then each land. The rail
down the left joins the rows; a round that asked for changes joins the next one with the dashed
"changes asked" line, so rework reads as a loop down the rail as well as the coil on the track.
Clicking a round opens its findings and, when it waited for a person, why. The latest reviewed
round is open to start with. Under that: "Waiting on" and "Blocks" with a way to add or remove a
blocker, the facts, the actions, the body, and the text timeline, collapsed (the last three now
live on the rail, the timeline under the journey).

## The Inbox

![The decision Inbox, light](img/inbox-light.png)

The Inbox is a list and the decision picked (`src/Inbox.tsx`, built by `decisionsOf` in
`src/decisions.ts`). The list groups decisions by kind (questions first) and gives each one row:
the kind's mark (a filled "?" for a question, a ringed mark otherwise, red for conflicts and
stopped lands), the key and title, and under them who and what last happened, with a teal dot when
it went well and a red one when it did not. The decision picked opens on the right: its kind in
the attention ink, the title, the full track, a card with the agent and what it asks or why it
needs you, and the action in place. The first decision is open until you pick another.

| Kind         | When                                                  | Headline                  | Action in place |
| ------------ | ----------------------------------------------------- | ------------------------- | --------------- |
| Question     | an open `ask`                                         | the question              | its options, or an answer in words |
| Approval     | an issue in Needs You                                 | why it needs a person (the review's reasons) | Approve, Send back to Todo, Open the worktree |
| Conflict     | a worktree with a rebase stopped on a conflict        | the rebase and its base   | Open the worktree |
| Waiting      | an agent session waiting for input in its terminal   | what it asks              | Open the worktree |
| Land stopped | the track is stuck at Landing and no agent is working | why the land stopped      | Open the worktree, Send back to Todo |

Within a kind, decisions are ordered by urgency (the issue's `urgency`, so a blocker of urgent
work rises), then by how long they have waited. The header says how many there are and the
oldest. The nav count is the same list (`useDecisionDetails` runs in the app). When nothing
waits, the page says so and lists what the agents are doing instead. The design's keys to approve
(A), answer (1–3) and send back (T) belong to NIT-54; the footer lists only keys that work.

## Tokens (`src/styles/tokens.css`)

Use roles, never raw colors. Both themes define every role.

- **Surfaces:** `--bg` (page), `--surface` (cards, panels, tables), `--sunken` (sidebar, the
  issue rail, code), `--hover` (also a raised box: a selected row, a flow box with issues),
  `--line`, `--line-strong`. A warm graphite neutral, not the stock cool gray.
- **Text:** `--fg`, `--fg-2` (secondary), `--fg-3` (faded: done, stale, hints).
- **Attention:** `--attention` (fills), `--attention-ink` (text on surfaces), `--attention-wash`
  (row backgrounds), `--on-attention` (text on the fill). Only for what needs the human.
- **Live:** `--live` (teal), an agent moving the work, and `--live-wash`. **Failed:** `--fail`
  (red) and `--fail-wash`.
- **Stations:** `--st-todo`, `--st-started`, `--st-working`, `--st-gate`, `--st-review`,
  `--st-needs` (= attention), `--st-landing`, `--st-done`, plus `--st-stuck` (= fail),
  `--st-changes`, `--st-closed`. Every agent station is `--live`; queued and done are `--fg-3`.
- **Vendors:** `--vendor-claude`, `--vendor-codex`, `--vendor-cursor`, `--vendor-human`,
  `--vendor-other`, `--on-vendor`.
- **Severity:** `--sev-p0`, `--sev-p1` (red), `--sev-p2`, `--sev-p3` (ink).
- **Type:** `--font` (Geist) and `--font-mono` (Geist Mono, only for data: keys, branches, paths,
  hashes, times and counts), bundled from `@fontsource-variable` so the app needs no network.
  Sizes `--text-xs` 11 to `--text-2xl` 34 (the glance numbers), `--weight-medium` 500,
  `--weight-strong` 600.
- **Space and shape:** `--space-1` to `--space-6` on a 4px step; `--radius-sm` 4, `--radius` 6,
  `--radius-lg` 10.
- **Motion:** `--ease`, `--dur-fast` 120ms, `--dur` 200ms, `--breathe` 2.4s. Respect
  `prefers-reduced-motion` (ui.css turns breathing off).
- **Overlays:** `--scrim` behind the palette and the issue sheet, `--shadow-float` under the
  palette, `--shadow-panel` beside the sheet.

The primary button is ink (`--fg`), not a brand color, so the attention color stays free.

## The Overview, the home screen

The home view was called Pulse until NIT-61; the code keeps that name (`src/Pulse.tsx`, the
`pulse` command), and `#pulse` links still open it. Its label is `HOME_LABEL` in `src/util.ts`.
It is the reference for a whole view built from these parts. It answers, in order:

1. **The glance:** four readouts in one strip (need you, agents moving, stuck, done today), each a
   large number in its role color (attention, live, fail, ink) with what it counts under it, as
   links, marks or tokens; done today carries landed per hour. A zero turns `--fg-3` and says so
   in words ("Nothing needs you"). The header says when you were last here and what happened since.
2. **Right now:** every agent with a session open, one row each: who and where, the issue, its
   mini track, what it is doing (or how long it has waited on you) and for how long. Working first,
   then waiting, then idle and stale, faded.
3. **One flow per project:** a box per station holding its issues as chips (glyph, key, agent).
   A box with issues is raised; Needs you with issues is washed in amber; an empty one is a dashed
   outline. Stuck work (a stuck track or a rebase conflict) shows a red glyph and key in its box,
   with the reason on hover; quiet work fades. Three chips a box, then "+N more". Below 960px the
   boxes become rows.
4. **Since you were last here:** counts that filter a feed (landed, filed, stuck, review rounds,
   questions) and landed and submitted per hour over 24 hours. The last-seen time is saved when you
   leave the Overview (`localStorage`), not while you read it.

Its data is one `pulse` command (`crates/nitely-core/src/pulse.rs`), which reads only the database.
Git state (rebase conflicts) and live sessions come from the snapshot the app already holds.

## Activity lanes

![Activity over 12 hours, light](img/activity-light.png)

The Activity view answers "what did the agents do while I was away". One lane per agent session,
grouped by project (projects with someone waiting on you first), time left to right, `now` at the
right edge. Above the lanes, a band of how many agents were working (and, in the attention color,
waiting on you) over the window.

- **Segments** are the session's state: working (`--live`, thick), waiting for you (`--attention`,
  thick), idle (thin), and stale, a dashed hairline once nothing was recorded for 30 minutes. A
  gap with no segment means the session was not running.
- **Before recording started** (session states are kept from the version that added them), the
  window is hatched and lanes show only "seen" stretches: the session was active, its state is
  unknown. The axis says when recording started. Never guess states for that stretch.
- **Marks** are `EventMark`s. Hover says what, which issue, when; a click opens the issue.

![Activity over 24 hours with the stretch before recording, dark](img/activity-24h-dark.png)

## Building a new view

1. **Start from the question the view answers** and put the answer in its header: a one-line
   glance (counts with `StateDot`s or a `Meter`). If nothing needs the human, say nothing
   about attention.
2. **Lay out by state, not by text.** Lanes, columns or a track: position says where things are.
   Sort what needs the human first, then live work, then the rest; fade what is done or stale.
3. **Use the primitives.** Issues are `IssueToken`s, agents are `AgentMark`s, progress is
   `StationTrack` or `StationGlyph`, from `trackOf`. If a primitive is missing a state, add it to
   the primitive (and to `src/dev/Kit.tsx`), not to your view.
4. **Detail on demand.** Hover `title`s for exact times and labels; clicking an issue opens the
   panel (`onOpenIssue`). Do not repeat in text what the shape already says.
5. **Every list can be empty, and every read can be slow or fail.** Use `EmptyState` with what
   would fill it, `<Loading />` until the first data, and an `error` line when the read fails.
6. **"Need you" means one thing.** It is the Inbox's list, `decisionsOf` (src/decisions.ts):
   questions, approvals, conflicts, waiting agents, stopped lands. App computes it once; the nav
   badge, the Overview and Now show that number. A view scoped to one project (the branch map) may count
   its own rows, but never under a different name for the same thing.
7. **Check it in the preview** (below) in light and dark, at 1280px and 375px, before you submit.
   Add fixture data in `src/dev/fixtures.ts` if your view needs some that is not there; keep it
   consistent with the stories.

Avoid: pills or chips around labels (a vendor's setup status is the one pill), cards with a colored
edge bar, shadows or glows on cards (overlays have one), gradients, icons in tinted boxes, a second
accent color, all-caps tracked labels (the kind over the Inbox's open decision is the one), mono for
prose, and animating anything that is not live.

## The sidebar and Now

The sidebar holds the mark and name, "Go to…" (the palette), the four views with their counts
(Inbox in a filled amber badge, Now in teal figures), the projects with a dot per agent session in
their repos (amber waiting on you first, then teal working, at most five), "+ Add project", and at
the foot Agent setup with a dot per installed agent (teal up to date, amber an update is ready, red
needs a look) and the line "Local · live".

Now is a card per worktree, two columns, grouped by repo: the agent and branch, the issue, its mini
track with the station in its color and the branch's git state, then its last three things (what
the agent is doing, the last commit, when the session started). A card waiting on you is washed in
amber; idle and stale ones fade.

The app icon (`src-tauri/icons/icon.svg`, sizes made with `npx tauri icon`) is the same mark as
the sidebar's: three stations on a line, on the dark graphite tile.

## The project views

![The board, light](img/board-light.png)

![The branch map, dark](img/branches-dark.png)

A project page has three views behind one switch (remembered): **List** (issues grouped by state),
**Board** and **Branches**, and **Settings** beside them (never remembered as the default).

- **Settings** (`src/ProjectSettings.tsx`, `crates/nitely-core/src/projects.rs`): the project's
  name; its key, where a new key re-keys every issue (NIT-12 becomes NX-12, with its timeline,
  reviews, blockers and asks) and the old key stays an alias, so `nitely://issue/NIT-12` links, old
  branches and pull requests still find the issue, refused while one of its issues is checked out
  in a worktree or mirrored from another tracker; and removing it, which deletes its issues only
  when the key is typed to confirm. Repos and machine tokens that name the project follow the new
  key, or name none after a removal. The CLI has the same: `nitely project rename|rekey|remove`.

- **Board.** One column per station, in track order; each head is the station's dot, its name and
  its count over a rule. Empty columns fold to a dashed strip with the name running down it. Cards hold the
  `IssueToken`, priority, `AgentMark` with live state, title, at most one flag (rebase stopped,
  asked you, waiting, stuck reason, blocked by), kind, `PrMark`, review rounds and age in the
  column. Needs-you cards are washed in attention, stuck ones get a stuck-tinged edge, blocked
  ones a dashed edge. Done keeps the last 7 days. In-flight issues load their timelines so
  Started, Gate and stuck are exact.
- **Branches.** Per repo: the integration branch as a spine (with the main checkout's state),
  every task branch forking off it, then **Loose ends**: worktrees with no issue or no branch,
  left after their issue closed, or idle 2+ days, and branches with no worktree. A solid fork and
  filled node is a worktree; a dashed fork and hollow node is a branch alone; a detached worktree
  has no fork. The node is attention when it waits on you, live while an agent works.

## Keys

- **Ctrl+K** (Cmd+K on a Mac), or "Go to…" at the top of the sidebar: the palette. Type part of a view,
  project or issue; arrows pick, Enter goes, Esc closes. An exact key ("nit-12") opens that issue.
  Results come in groups ("Go to", "Issues") with a count of matches.
- **j / k** move focus to the next or previous item on the page (list rows, the Overview's rows and
  chips, board cards, the Inbox's decisions, lanes, branch lines, Now's cards, the feed); **Enter**
  opens it (on an Inbox row, focus picks it and Enter opens its issue). They do nothing while you
  type in a field.
- **Esc** closes the issue panel.

**Links.** A native issue's `url` is `nitely://issue/NIT-14` (crates/nitely-core/src/issues.rs,
`issue_link`). The app registers the `nitely` scheme (Tauri's deep-link plugin; the installer at
install time, and every run of the app for the current user on Windows and Linux), and a link opens
that issue's panel, in the running window if there is one. Inside the app, issues open the panel
directly; the link is for everywhere else: agents' replies, commits, pull requests and notes.

A new view joins j/k by adding its item selector to `ITEMS` in `src/Palette.tsx`; the items must be
focusable (a button or link) so Enter works and the focus ring shows.

## The browser preview

`npm run dev` in a plain browser runs the app on sample data: `src/backend.ts` sends every
`invoke` to `src/dev/fixtures.ts` when there is no Tauri. A production build drops the fixtures,
and inside Tauri nothing changes.

- Agents: `.claude/launch.json` has a `web` configuration (port 5173), so the Browser pane's
  `preview_start` with name `web` opens it. Or run `npm run dev -- --port <port> --strictPort`.
- Routes are in the hash: `#overview` (the default; `#pulse` still works), `#inbox`, `#now`, `#activity`, `#project/NIT`,

  `#agents`, `#add`, `#kit`, and `@KEY` opens an issue panel (`#now@NIT-29`). `?hours=1|6|12|24`
  opens Activity at that window. A project's view follows its key: `#project/NIT/list`,
  `#project/NIT/board`, `#project/NIT/branches`; without one, the last view picked.
- Theme: the switch at the bottom of the sidebar, or `?theme=light|dark` in the URL, or the
  browser's color scheme. `[data-theme]` on `<html>` forces a theme.
- The fixtures are four projects (NIT, AND, HAR, and OLD with no issues, for trying Settings), 40 issues across every state, nine agent
  sessions (working, idle, waiting, stale), pull requests with checks, review rounds with
  findings, two open asks and two stuck worktrees. The repos also carry sprawl for the branch map:
  a worktree left after its issue landed, a spike with no issue, a detached worktree, branches with
  no worktree (merged, canceled, a person's own) and worktrees idle for days. Each issue is written as a short story
  (`filed, claim, work, gate, review, changes, ...`) and its timeline, reviews, pull request and
  worktree are generated from that, so they agree. Answering an ask or moving an issue works and
  changes the data until reload. Activity lanes come from the same stories (one session per issue
  an agent worked on), with states recorded only for the last 19 hours, so the 24h window shows
  the not-recorded stretch.
- If the preview suddenly fails with `reading 'invoke'` after a rebase or land, restart the dev
  server (with `--force`): Vite kept the files as they were mid-rebase.
- **Live data, read-only:** add `?data=live` (or use "Use live data" at the bottom of the sidebar) and
  the preview reads your real Nitely database instead of the fixtures, to check a view against real
  work. The dev server relays each read to `nitely ui <command> <args>` (vite.config.ts), which runs
  this checkout's `target/debug/nitely` (build it with `cargo build -p nitely-cli`; `NITELY_BIN`
  overrides). `nitely ui` runs only reads and refuses every command that changes Nitely itself
  (approve, answer, move, add a project, agent setup, sync), so the preview can never act for the
  human. It opens the database read-only (SQLite refuses writes; nothing is created or migrated),
  and agent setup comes without config file contents or errors, which can hold credentials. A
  banner says the page is live and read-only.
- **Several agents run previews at once**, so keep yours apart:
  - Use your own port (`--port <n> --strictPort`), and your own Browser pane tab: open it with
    `tabs_create` and pass its `tabId`, because the pane is shared and another agent may navigate
    the active tab to its own server mid-check.
  - Keep scratch files (screenshot scripts, shots, browser profiles) in a folder named after your
    issue, such as `<scratchpad>/nit-29/`; the scratchpad folder is shared between sessions.
  - Stop your dev server before `land`, or the worktree cannot be removed.
- **Full-resolution screenshots without the pane:** headless Edge, always with a profile folder of
  your own, or Edge hands the URL to another agent's running instance and you get its page:
  `msedge --headless=new --user-data-dir=<your folder>/profiles/<shot> --window-size=1280,900 --virtual-time-budget=5000 --screenshot=<your folder>/out.png "http://localhost:<port>/?theme=dark#now"`.
  With its own profile the command returns before the file is written, so wait for the file.
  Headless Edge will not go narrower than about 500px; check 375px in the Browser pane's mobile
  viewport (`resize_window` preset `mobile`) instead.
