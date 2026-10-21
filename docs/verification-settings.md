# Verification settings and the landing model

This page lists every setting that decides what happens to a change between `submit` and main, as
of goal 19's start (main a1f4241). It then describes the landing model that replaces them: one
landing path, goal branches with checkpoint lands, and three presets. The build cards (NIT-491 to
NIT-499) implement this page, and the cleanup card removes what it marks as retired.

Rust paths are under `crates/nitely-core/src/`.

## Inventory

### Where policy lives today

There are four layers. The rule that settles most conflicts: whenever a review-mode field is set,
`project_modes::apply_project` (`project_modes.rs:105`) writes it over the parsed playbook. Every
submit, land, queue, gate, verification and backstop decision calls it.

| Layer | Where | Who writes it |
|---|---|---|
| Project review mode | DB `project_review_mode` (mode, overrides JSON, revision) | the person, in Settings > Review mode |
| Legacy YOLO switch | DB `project_yolo` (enabled, revision) | written again on every mode save (`project_modes.rs:93`) |
| Playbook | `.nitely/playbook.toml`, read from the integration commit; also a stored copy and lane-lent copies (`playbook.rs:559`) | agents propose, the person approves |
| Defaults in code | struct defaults, app `RuleDefaults` (`rules.rs:60`) | nobody: they apply whenever a key is missing |

### The project review mode

Presets are at `project_modes.rs:43`. Custom takes its fields from the overrides, and a field left
unset falls through to the playbook.

| Option | YOLO | Fast | Balanced | Thorough | Applied to the playbook as |
|---|---|---|---|---|---|
| test_depth | off | basic | risk | full | `verification.mode` active/shadow (`verification.rs:339`); plan commands (`:355`); gate skipped (`project_yolo.rs:40`) |
| pre_review | off | off | batched | individual | `review.enabled`; `review.batch` All or Off (`project_modes.rs:107`) |
| land_batch | on | off | on | off | `land.batch` Waiting or Off (`:108`) |
| shared_lanes | on | on | on | on | creates a default `[lanes]` (1 lane, 30 min) or removes it (`:109`) |
| full_backstop | **off** | **off** | on, 5 lands / 30 min | **off** | `verification.backstop.enabled/landings/delay_seconds` (`:110`) |
| audit_percent | 0 | 0 | 10 | 0 | `verification.audit_percent` (`:111`) |
| periodic review | off | 5 lands / 30 min | 5 lands / 30 min | off | read only from the mode (`periodic_review.rs:176`) |

### The playbook

| Key | Default in code | NIT today | Read by |
|---|---|---|---|
| `[gate] commands`, `[[gate.steps]]` (`when_changed`, `unless_only`), `retries` | [], [], 1 | npm test, npm run build; cargo test and clippy unless only web/docs changed | `run_gate_inner` (`playbook.rs:1421`); a reduced plan replaces commands and drops steps |
| `[review] enabled` | true | unset | `review.rs:85`, overridden by pre_review |
| `[review] reviewer`, `commands`, `timeout_minutes`, `require_cross_vendor`, `focus` | Codex for Claude, Claude for the rest; 20 min | everything to Codex; focus set | `review.rs:118`; `RuleDefaults` fill gaps |
| `[review] flags` | [] | none | any flag forces Full (`verification.rs:283`) |
| `[review] batch`, `batch_window`, `batch_max`, `small_lines`, `small_files`, `max_lines` | off, 5m, 3, 100, 5, 1500 | **shadow**, 5m, 3 | `review_batch.rs:108`, `submit.rs:244` |
| `[approval] always_human`, `human_paths`, `blocked_paths` | false, [], [] | none | `playbook.rs:461`; any hit forces Full |
| `[land] by` | author | unset | `human` adds an approval reason |
| `[land] batch`, `batch_window`, `batch_max` | off, none, 5 | waiting, 5 | `lands_alone` (`land_queue.rs:179`) |
| `[[land.after]]` | [] | npm install; cargo install | after a land; a failure marks main known-broken |
| `[lanes] count`, `keep`, `heavy`, `timeout_minutes`, `executor` | no lanes | 3 lanes, 75 min | gate routing (`playbook.rs:1444`); land batching needs lanes |
| build slots | from the machine; `NITELY_BUILD_SLOTS`, then config `build_slots` | machine | lane admission (`lanes.rs:1284`) |
| `[worktree] dir`, `setup`, `mode` | full | light, npm ci | `start_task`, setup before the gate |
| `[verification] mode`, `version`, `prose`, `comments`, `display_copy`, `protected`, `basic` | "", 0, [] | active, 1, patterns, `tsc --noEmit` | classifier (`verification.rs:272`), from the base commit only |
| `[verification] audit_percent` | **10** | 10 | `verification.rs:388`, `verification_audit.rs:150` |
| `[verification.backstop] enabled`, `landings`, `delay_seconds`, `nightly_seconds`, `auto_revert`, `reversible_paths`, `irreversible_paths` | false, 5, 1800, 86400, true, [], [] | enabled = false | `verification_backstop.rs:248`; auto-revert needs every path in `reversible_paths` (unset: every path but the built-in list, NIT-496; `[]`: never) |
| `[release] tag`, `commands`, `stamp_paths` | none: release refuses | v* | `release_verification.rs:61`; always the Full gate plus these |

### Other inputs

| Setting | Where | Effect |
|---|---|---|
| Affected-tests map | `.nitely/affected-tests.toml`, read from the base commit (`affected_tests.rs:12`) | picks component checks; any error or unowned file gives Full |
| Review level | issue field (inert, contained, standard, sensitive), raised by path rules | inert: basic only, no pre-land review; contained: one component; standard: basic plus mapped checks; sensitive: Full. Undeclared, protected, add/delete/rename, more than 10 files or 400 lines gives Full |
| Disabled rules | DB, from confirmed audit escapes | a matching lowering rule forces Full |
| Land-queue stack | `<lanes>/stack` worktree (`land_queue.rs:1`) | what waited is stacked, gated once at the tip, bisected on failure. This is the only "stack" in Nitely: there are no user-stacked branches |
| Review shadow | `[review] batch = "shadow"` | reviews alone, then again in record-only batches |
| Verification shadow | `[verification] mode = "shadow"` | classifier records advice only |
| Periodic review | mode only | non-blocking review of main ranges |

### Where the layers disagree today

| # | What happens | Evidence |
|---|---|---|
| 1 | YOLO, Fast and Thorough set audit_percent 0, which beats the playbook's 10: no audits run. | `project_modes.rs:50`, `playbook.toml` `audit_percent = 10` |
| 2 | test_depth rewrites `verification.mode`: Risk/Basic force active, Off/Full force shadow, whatever the playbook says. | `verification.rs:341` |
| 3 | Fast's Basic overrides every risk floor: a sensitive or protected change runs only `tsc --noEmit`. | `verification.rs:360` |
| 4 | YOLO (and Fast, Thorough) switch the backstop off, and release then stays blocked on stale health. | `project_modes.rs:49`, `verification_backstop.rs:161` |
| 5 | Balanced switches the backstop on when the playbook says off, but nightly, auto-revert and paths still come from the playbook. | `project_modes.rs:19` lacks those fields |
| 6 | An explicit test_depth with the backstop off skips the backstop-health precondition, so Fast lands reduced changes with no backstop, no audit and no review. | `verification.rs:414` |
| 7 | pre_review overrides `[review] batch`: Individual or Off turns NIT's shadow measurement off; Batched means All, never Small. | `project_modes.rs:107` |
| 8 | land_batch on means `waiting`, so a playbook `all` and its window are dropped. | `project_modes.rs:108` |
| 9 | shared_lanes off silently disables land_batch on. | `land_queue.rs:186` |
| 10 | Any preset removes the inert review exemption (it needs pre_review unset). | `submit.rs:219` |
| 11 | Turning YOLO off through the legacy call resets the project to Custom with nothing set. | `project_yolo.rs:25` |
| 12 | Reduced plans apply only to single-change gates, so every land batch runs Full. That is right for a batch, but it is why NIT-473 ran the Rust suite twice. | `playbook.rs:1411` |
| 13 | Docs lag: `verification.md` and `verification-backstop.md` say reduction is advice only, `yolo.md` describes a standalone switch, and `batching.md` and `build-lanes.md` do not say a mode overrides them. | docs |

The root problem: policy is split between the mode and the playbook, the mode overwrites some
playbook keys and not others, and the presets disagree about the backstop, the one safety net that
makes reduced checks safe.

## The landing model

### 1. Targets

A change lands into its **target**:

| Change | Target | Checks (item level) | Review | PR |
|---|---|---|---|---|
| Issue in a goal with a goal branch | the goal branch `goal-<id>` | compile, lint, affected tests | none | none |
| Issue with no goal (or a goal without a branch) | main | per preset (below) | per preset | yes |
| **Checkpoint**: a goal branch | main | full: one full run (CI when available, NIT-493) | one review of the slice's diff | one PR for the slice |

- One path. `submit` and `land` serve every hop. The target and the check level are the only
  things that vary. A checkpoint is one entry in the same land queue as any other change.
- **Item hop.** `start_task` branches from the goal branch. `land` rebases onto the goal branch,
  runs the item checks in a lane, and fast-forwards the goal branch. The goal's other items get it
  at once.
- **Checkpoint hop.** The goal branch already contains main (see 3). The checkpoint gates its tip
  with the full gate and has the other vendor review the whole slice. If main has not moved, it
  fast-forwards main; if main has moved, it merges main in first and gates that. The person's
  approvals apply here. A slice that touches a `human_paths` file needs their approval of the
  slice, signed as one patch.
- **Goal branch lifetime.** Created from main when the goal's first item starts. It ends when the
  goal closes and its last checkpoint has landed.

### 2. Presets

Policy lives only in the review mode. A preset sets every value below; Custom is the escape hatch
and is the only way to switch a safety net off, shown in red on the "How a change lands" strip.

| | **Fast** (default) | **Careful** | **Release** |
|---|---|---|---|
| Goals use branches | yes | yes | no: every item lands on main |
| Item hop: checks | compile, lint, affected tests | compile, lint, affected tests | full gate |
| Item hop: review | none | each item, non-blocking (findings become issues on the goal) | each change, blocking |
| Checkpoint trigger | a coherent slice (the coordinator calls it), else 10 items, else daily | a coherent slice, else 5 items, else daily | n/a |
| Checkpoint: checks | one full run | one full run | n/a |
| Checkpoint: review | one review of the slice, P0/P1 block | one review of the slice, P0/P1 block | n/a |
| No-goal issues: checks | compile, lint, affected tests | compile, lint, affected tests | full gate |
| No-goal issues: review | one review per arrival batch, of its combined diff, peer strength, blocking: nothing lands unreviewed | each change, blocking | each change, blocking |
| No-goal issues: land | arrival-order batches of up to 5, one full run per batch, bisected | the same | alone |
| Backstop on main | on, in CI: every 5 lands or 30 min, auto-revert | on, the same | on, every land |
| Audits of reduced checks | 10% | 10% | n/a (nothing reduced) |
| Periodic review of main | off (the checkpoint review covers it) | off | off |
| Human approval | `human_paths` from the playbook, at the hop into main | the same | the same, plus every change |

Confirmed by the person on 2026-10-21, with one change: issues with no goal are still reviewed,
one review per arrival batch, so nothing lands unreviewed.

Rules every preset keeps:

- The risk floors always win: a protected, flagged or unmapped change runs the full gate on any
  hop, so Fast can never run only `tsc` on a sensitive file (disagreement 3).
- The backstop is on in every preset (disagreements 4 and 6).
- Changing the mode never fails work queued under the old one (NIT-487): queued work finishes under
  the policy it was submitted with.

Migration: YOLO becomes Fast, Balanced becomes Careful, Thorough becomes Release. Custom keeps its
overrides, minus the retired keys. The app shows each project what changed.

**What the playbook keeps** (facts about the repo, never policy): the integration branch, the gate
commands and steps (the full gate), the quick commands (today's `verification.basic`), the
affected-tests map, protected, human and blocked paths, review focus and the reviewer map, lanes
(count, keep, heavy, timeout), `[[land.after]]`, which commands run in CI, auto-revert paths and
`[release]`. A playbook policy key that conflicts with the mode is ignored with a visible notice.

### 3. Keeping goal branches healthy

- **Main flows in.** After every land on main, main is merged into each open goal branch (a merge,
  never a rebase, since the branch is shared). A clean merge is gated at item level. A conflict, or
  a gate that fails after the merge, goes to the item that last touched the file, or to a new
  issue on the goal when no item did. So a conflict shows up on the item that caused it, not at
  the checkpoint.
- **A red goal branch blocks only its goal.** Its item lands wait. Other goals and no-goal issues
  carry on. The goal row shows green or red, the items merged since the last checkpoint, and the
  next checkpoint.
- **Checkpoint failure bisects over the goal's items.** The items are the goal branch's
  first-parent merges since the last checkpoint. Bisection finds the item at fault, reverts it on
  the goal branch, sends it back to its agent and retries the checkpoint without it. That costs at
  most 1 + 2*ceil(log2 k) full runs, as the land queue does today.
- **Evidence** is recorded for every item hop and every checkpoint (NIT-469). Item evidence never
  counts as full evidence for main.

### How goal branches are built (NIT-495)

`crates/nitely-core/src/goal_branch.rs`, inside the existing hops rather than beside them:

- **Turning it on.** `nitely goal branch <id>` (MCP `goal_branch`) gives a goal its branch,
  `goal-<id>`, made from main when its first item starts. `--branch <name>` adopts a branch that
  exists already, which is how a goal run by hand becomes a real goal branch:
  `nitely goal branch 19 --branch goal-19`. `--items <n>` sets when a checkpoint is due (10). Under
  Fast, NIT-491's preset turns it on for every goal; until then a goal opts in.
- **Item hop.** `playbook::start_task` branches an item from the goal branch. `land` sends it to
  the same `run` as a land on main, with the goal branch as its base: rebase, item checks
  (`git diff --check`, `[verification] basic`, and the affected-test map's checks; the full gate
  when the map cannot place a file), then the goal branch's ref moves and is pushed. It takes the
  goal's own lock, never the repo's land lock, so lands on main never wait for it. `submit` on an
  item lands it, since there is no review or pull request to wait for. The item plan is recorded as
  the item's verification evidence, marked as not evidence for main.
- **Main flows in.** After every land on main (alone or through the queue), `flow_main_in` merges
  main into each open goal branch, gates the merge at item level and moves the branch; a goal
  branch that holds nothing since main took it in just moves to main. An item land merges main in
  first when the branch is behind. A conflict or failed checks turns the branch red, files the fix
  on the goal and notes the item that last touched the files. A red branch takes only the item that
  contains the main that turned it red; that land turns it green.
- **Checkpoint.** A checkpoint is an issue on the goal whose branch starts at the goal branch's
  head. One is filed by itself when `--items` items are waiting or the oldest has waited a day, and
  the coordinator opens one for a coherent slice with `nitely goal checkpoint <id>` (MCP
  `checkpoint`). It is submitted and landed like any issue: one review of the slice, one pull
  request, one full gate (in CI for the playbook's `ci_commands`, NIT-493). It lands alone through
  the land queue's lock, merging main in rather than rebasing. When the full gate fails, the land
  bisects over the items' commits on the goal branch, reverts the one at fault there, sends it back
  to Todo and opens the next checkpoint without it.
- **The goal row** shows the branch: items since the last checkpoint, green or red with its fix,
  and the next checkpoint. Its tooltip counts items landed, checkpoints, full runs and reviews, the
  before-and-after measure.

### 4. What gets retired

For removal by NIT-499, each with a migration and a test that the replacement covers its case:

| Retired | Replaced by |
|---|---|
| `project_yolo` table, the YOLO mode and `tests_off` gate skipping | Fast; Custom for anyone who really wants tests off |
| Balanced and Thorough mode names | Careful and Release |
| Review batching as a setting: `[review] batch`, `batch_window`, `batch_max`, `small_*`, `max_lines` | the checkpoint review of the slice; under Fast, one review of each no-goal arrival batch (the land queue's batch, not a separate review window) |
| Review shadow batches (`batch = "shadow"`) | removed: the measurement is done |
| Verification shadow: `[verification] mode` and `version` as switches | the preset decides; the classifier always runs |
| pre_review, land_batch, shared_lanes, full_backstop, audit_percent and periodic as separate mode fields | derived from the preset; Custom keeps them as overrides |
| `[review] enabled`, `[land] batch*`, `[verification] audit_percent`, `[verification.backstop] enabled/landings/delay_seconds` in the playbook | the preset (the playbook keeps only auto-revert paths and nightly cadence) |
| The land-queue stack for goal items | the goal branch; arrival-order stacking stays only for no-goal issues and for checkpoints entering the queue |
| Periodic review of main as a default | the checkpoint review; Custom only |
| `docs/yolo.md`, `docs/verification-shadow.md`, and the playbook-only parts of `docs/batching.md` | this page and `docs/review-modes.md` |

When the goal ends there are three presets, one landing path and two hops. That replaces five
modes, two shadow switches, two kinds of batching and a parallel YOLO table.

### Where the retirement stands (NIT-499)

| Retired | State |
|---|---|
| `project_yolo` table and the switch | Dropped by a migration; the app, CLI and MCP no longer read or set it. Custom with tests and review off is what YOLO was. |
| YOLO, Balanced and Thorough names | Migrated by NIT-491; the old names still parse for stored rows. |
| Review shadow batches | Gone: `batch = "shadow"` reads as off and submits queue nothing record-only. `nitely batches compare` reads the old rows. |
| Playbook policy keys | Ignored, with a notice: `[review] enabled` and `batch`, `[land] batch`, `[verification] mode` and `audit_percent` (`version` stays: it is the policy format), `[verification.backstop] enabled`, `landings` and `delay_seconds`. |
| Custom falling back to the playbook | Custom's unset values are Nitely's defaults (`project_modes::builtin`): the full gate, a review of each change, landing alone, no goal branches, 10% audits, the full suite on main off. |
| The land-queue stack for goal items | Nothing to remove: NIT-495 lands items into the goal branch and gates each checkpoint alone. |
| Periodic review as a default | Off in every preset (NIT-491); Custom only. |
| `docs/yolo.md`, `docs/verification-shadow.md` | Deleted; `docs/review-modes.md` and `docs/batching.md` describe the presets. |

Kept for now, as follow-ups:

- Done in NIT-509: Fast's batch review is the land queue's batch, reviewed once before the run's
  gate, and `[review] batch_window`, `batch_max`, `small_lines`, `small_files` and `max_lines` are
  retired (ignored with a notice). `[land] batch_window` and `batch_max` still tune the land queue.
- The "light" review exemption for inert changes needed the review left to the playbook; no
  project has that now, so the exemption no longer applies, and its code can go.

Concepts before and after:

| Before | After |
|---|---|
| Five modes (YOLO, Fast, Balanced, Thorough, Custom) and a separate YOLO switch | Three presets and Custom |
| Policy in the mode and in the playbook, each overruling the other per key | Policy in the mode only; the playbook states facts |
| Review batching as a playbook setting, plus a shadow measurement | One review per arrival batch under Fast; the checkpoint review for goals |
| Verification shadow and active switches in the playbook | The preset decides; the classifier always runs |
| A backstop that some presets switched off | On in every preset; off only by an explicit Custom choice |
