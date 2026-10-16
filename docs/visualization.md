# Visualization: how Nitely shows work

**Status:** set by NIT-24, 2026-10-16. Every view builds on this: Pulse, the issue track, the
decision Inbox, activity lanes, the board and the branch map. If a view needs something this file
does not cover, extend the file in the same change.

Andrew runs many agents across several projects. Nitely is useful only if he can tell, in a few
seconds, **what needs him, what is moving and what is stuck**. Showing that clearly is the
product.

![Now with an issue's track open, light](img/now-and-track-light.png)

![The visual kit, dark](img/kit-dark.png)

## Principles

1. **Answer at a glance.** The top of every screen says whether anything needs Andrew, what is
   moving and what is stuck, before any detail. One line is enough: see the `Glance` line on
   Now (`● 2 working  ● 2 need you  ○ 2 stale`) and the summary on a project page.
2. **Shape before text.** State is shown by position, color and form: where a token sits on the
   track, which shape a mark has, whether a dot breathes. Text is there to confirm, on hover
   (`title`) or in the issue panel. If you need a sentence to say what state something is in,
   draw it instead.
3. **One vocabulary everywhere.** An issue is always an `IssueToken`. An agent is always an
   `AgentMark`. Progress is always the lifecycle track. A view never invents its own badge for
   these.
4. **Calm by default.** There is one attention color, and it is used only for what needs the
   human. Quiet things fade: done, canceled and stale go to `--fg-3` or lower opacity. Live work
   moves a little (the breathing dot); nothing else animates.
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
  rounds.
- **Stuck:** the last gate failed, the last land stopped, or the review did not finish, and
  nothing has passed since. The current station turns `--st-stuck` and the line onward breaks
  into dashes. A stuck worktree that waits on the human (a rebase conflict, an agent waiting for
  input) is the attention color instead, because it needs him.
- **Asked you:** an open `ask`. An attention dot on the current station (and on the glyph).
- **Closed:** Canceled or Duplicate. The whole track fades and the station reads Canceled or
  Duplicate.

Stations before the current one that were never reached are **skipped** (small hollow ring); most
issues skip Needs you. All of this is computed in one place, the backend's
`crates/nitely-core/src/track.rs` (with its tests). It puts a `track` on every issue the app
receives (`issues`, `issue_detail`, `snapshot`, `pulse`): open issues from their timelines, closed
issues in lists from their state. On the frontend always call `trackOf(issue)` from
`src/ui/lifecycle.ts`, which returns that `track`; its `computeTrack` mirrors the Rust rules only for
the browser preview's sample data. A rule change goes into `track.rs` first, then `computeTrack`.
Never compute stations anywhere else.

## Primitives (`src/ui/`)

Import from `./ui` (or `../ui`). Open the browser preview at `#kit` to see each one in every state.

| Primitive      | What it shows                                                                 | Use it for |
| -------------- | ----------------------------------------------------------------------------- | ---------- |
| `IssueToken`   | Station glyph + key, optionally title and kind. Opens the panel (native) or Linear. | Any mention of an issue. |
| `StationGlyph` | The track folded into a 14px ring that fills as the issue advances.          | Dense rows where the key sits in its own column. |
| `StationTrack` | `size="full"`: every station, labels, times, loop, stuck. `size="mini"`: a 62px bar. | Full: the issue panel, a focused issue. Mini: rows that mix states. |
| `AgentMark`    | Vendor shape and color, with a notched live dot.                             | Any mention of an agent or the human. |
| `StateDot`     | A session's liveness on its own: working (breathes), waiting, idle, stale, ended. | Legends, glance lines, places without a vendor. |
| `Sparkline`    | A line or bars, oldest first.                                                 | Activity over time next to a number. |
| `Meter`        | Parts of a whole as one segmented bar.                                       | Open issues by phase, checks by result. |
| `CountBadge`   | A count: `attention` (filled), `live`, or `quiet`.                           | Nav items and section headings. |
| `EmptyState`   | An empty track, one sentence, what would fill it.                            | Every list or page that can be empty. |
| `PriorityMark` | Issue priority as three rising bars; urgent is red.                          | Issue rows. |
| `SeverityMark` | A review finding's P0 to P3. Only P0 and P1 are red.                         | Review findings. |
| `EventMark`    | Something that happened to an issue, as a 12px shape: ring started, dot commit, square gate, diamond review, target land, bar handoff, attention dot ask. Red only for failures. | Marks on a timeline (activity lanes), legends. |

Helpers: `livenessOf(session, now)` (adds "stale" after 30 minutes quiet), `vendorOf(actor)`,
`phaseOf(station)`, `stationColor(station, stuck)`, `describeTrack(track)` for hover text.

### Agents

| Vendor | Shape                  | Token              |
| ------ | ---------------------- | ------------------ |
| Claude | disc with C            | `--vendor-claude`  |
| Codex  | rounded square with X  | `--vendor-codex`   |
| Cursor | hexagon with a caret   | `--vendor-cursor`  |
| Human  | ring                   | `--vendor-human`   |
| Other  | disc with ?            | `--vendor-other`   |

Vendor colors are muted on purpose, so a screen full of agents stays calm and none of them
competes with the attention color. The shape carries identity as much as the color does. Live
state is the small dot at the bottom right: breathing `--live` when working, `--attention` when
waiting for the human, `--fg-3` when idle; stale marks fade to 45%, ended to 30%.

## Tokens (`src/styles/tokens.css`)

Use roles, never raw colors. Both themes define every role.

- **Surfaces:** `--bg` (page), `--surface` (cards, panels, tables), `--sunken` (sidebar, code),
  `--hover`, `--line`, `--line-strong`. A warm graphite neutral, not the stock cool gray.
- **Text:** `--fg`, `--fg-2` (secondary), `--fg-3` (faded: done, stale, hints).
- **Attention:** `--attention` (fills), `--attention-ink` (text on surfaces), `--attention-wash`
  (row backgrounds), `--on-attention` (text on the fill). Only for what needs the human.
- **Live:** `--live`, an agent working right now. Also the Working station.
- **Stations:** `--st-todo`, `--st-started`, `--st-working`, `--st-gate`, `--st-review`,
  `--st-needs` (= attention), `--st-landing`, `--st-done`, plus `--st-stuck`, `--st-changes`,
  `--st-closed`. The agent's stations step through one teal ramp; done fades.
- **Vendors:** `--vendor-claude`, `--vendor-codex`, `--vendor-cursor`, `--vendor-human`,
  `--vendor-other`, `--on-vendor`.
- **Severity:** `--sev-p0`, `--sev-p1` (red), `--sev-p2`, `--sev-p3` (ink).
- **Type:** `--font` (the system face), `--font-mono` (only for data: keys, branches, paths,
  hashes), sizes `--text-xs` 11 to `--text-2xl` 28, `--weight-medium`, `--weight-strong`.
- **Space and shape:** `--space-1` to `--space-6` on a 4px step; `--radius-sm` 4, `--radius` 6,
  `--radius-lg` 10.
- **Motion:** `--ease`, `--dur-fast` 120ms, `--dur` 200ms, `--breathe` 2.4s. Respect
  `prefers-reduced-motion` (ui.css turns breathing off).

The primary button is ink (`--fg`), not a brand color, so the attention color stays free.

## Pulse, the home screen

`src/Pulse.tsx` is the reference for a whole view built from these parts. It answers, in order:

1. **The glance:** four readouts (needs you, agents working, stuck, landed today), each a large
   number in its role color (attention, live, stuck, ink) with what it counts under it, as links or
   tokens. A zero turns `--fg-3` and says so in words ("Nothing needs you").
2. **One flow per project:** the stations as columns on one line, each open issue a card at its
   station (agent mark with live dot, key, two-line title). Todo and Done today show keys only,
   because queued and finished work should stay quiet. Stuck work (a stuck track, a rebase conflict
   from the board, or quiet for an hour at an agent's station) sits in a band under the stations,
   on `--bg`, with red borders and the reason in red; quiet cards fade instead. Below 960px the
   stations become rows.
3. **Since you were last here:** counts that filter a feed (landed, filed, stuck, review rounds,
   questions) and landed and submitted per hour over 24 hours. The last-seen time is saved when you
   leave Pulse (`localStorage`), not while you read it.

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
5. **Every list can be empty.** Use `EmptyState` with what would fill it.
6. **Check it in the preview** (below) in light and dark, at 1280px and 375px, before you submit.
   Add fixture data in `src/dev/fixtures.ts` if your view needs some that is not there; keep it
   consistent with the stories.

Avoid: pills or chips around labels, cards with a colored edge bar, shadows or glows, gradients,
icons in tinted boxes, a second accent color, all-caps tracked labels, mono for prose, and
animating anything that is not live.

## The browser preview

`npm run dev` in a plain browser runs the app on sample data: `src/backend.ts` sends every
`invoke` to `src/dev/fixtures.ts` when there is no Tauri. A production build drops the fixtures,
and inside Tauri nothing changes.

- Agents: `.claude/launch.json` has a `web` configuration (port 5173), so the Browser pane's
  `preview_start` with name `web` opens it. Or run `npm run dev -- --port <port> --strictPort`.
- Routes are in the hash: `#pulse` (the default), `#inbox`, `#now`, `#activity`, `#project/NIT`,
  `#agents`, `#add`, `#kit`, and `@KEY` opens an issue panel (`#now@NIT-29`). `?hours=1|6|12|24`
  opens Activity at that window.
- Theme: the switch at the bottom of the sidebar, or `?theme=light|dark` in the URL, or the
  browser's color scheme. `[data-theme]` on `<html>` forces a theme.
- The fixtures are three projects (NIT, AND, HAR), 40 issues across every state, nine agent
  sessions (working, idle, waiting, stale), pull requests with checks, review rounds with
  findings, two open asks and two stuck worktrees. Each issue is written as a short story
  (`filed, claim, work, gate, review, changes, ...`) and its timeline, reviews, pull request and
  worktree are generated from that, so they agree. Answering an ask or moving an issue works and
  changes the data until reload. Activity lanes come from the same stories (one session per issue
  an agent worked on), with states recorded only for the last 19 hours, so the 24h window shows
  the not-recorded stretch.
- If the preview suddenly fails with `reading 'invoke'` after a rebase or land, restart the dev
  server (with `--force`): Vite kept the files as they were mid-rebase.
- For full-resolution screenshots without the pane, headless Edge works:
  `msedge --headless=new --window-size=1280,900 --virtual-time-budget=4000 --screenshot=out.png "http://localhost:5173/?theme=dark#now"`.
