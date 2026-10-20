# Batching review and landing

How a project can review and land changes together instead of one at a time: each change alone,
small changes batched, or all of them batched. This is the design, measured against Nitely's own
history before anything changes in the product. Back to the [README](../README.md); the warm checkouts
the gate moves into are in [build-lanes.md](build-lanes.md), and asynchronous submit is
[NIT-212](nitely://issue/NIT-212).

## The short answer

1. **Asynchronous submit first.** Every agent today sits through its own gate and review. Handing
   the change in and moving on gives back all of that waiting, for every change, at no cost in
   review quality: 10.3 h of agents waiting on reviews over the 389 reviews recorded so far. No
   batching setting comes close. It is [NIT-212](nitely://issue/NIT-212), and batching review
   builds on it.
2. **A land queue that batches only what is already waiting.** When a land is running, the
   reviewed changes that arrive meanwhile wait and then go together: stacked, gated once, pushed
   once. With no window, it adds no delay. At this repo's load (gates of 85 to 115 s, the land
   lock busy about a quarter of the day) it almost never forms a batch, and it costs nothing.
   For a project whose gate takes 5 minutes, it cuts the median wait to land from 51 minutes to 3.
3. **Batching review is an option, not a default.** Reviewing three small changes in one call
   kept each finding on its own change and used about half the tokens of three calls. But only
   15% of this repo's changes are small, a 5-minute window would have saved 16 reviewer calls
   on 2026-10-16 while adding 56 s of waiting to each batched review, and severity drifted between
   runs. It is worth turning on for projects with many tiny changes, and only together with
   asynchronous submit, where the waiting no longer holds an agent.

Both settings default to `"off"`: each change is reviewed and landed on its own, as today.

## What the numbers say

From Nitely's database and this repo's git objects, read only, for 2026-10-16 and the first hours
of 2026-10-17. The method is at the end.

### Reviews are mostly not small

389 reviews with a measured duration, 331 of them with a diff size from git:

```
review time      median 85 s, p90 157 s, 10.3 h in all
lines changed    median 325, p90 1,477
```

```
small means            reviews       review median   asked for changes
<= 50 lines, 3 files    20   (6%)    48 s            0
<= 100 lines, 5 files   50  (15%)    56 s            6
<= 200 lines, 8 files  105  (31%)    64 s            18
```

A review costs about 65 s of fixed time plus 0.06 s per line changed (least squares over 322
reviews up to 2,000 lines): a 20-line change takes 48 s, a 500-line one 89 s, a large one 126 s.
The fixed part is what batching saves, and it is about one minute of the reviewer's time per call.

### Batching small reviews (<= 100 lines, 5 files)

```
window   reviewer calls   batches of 2+   added wait per review   reviewer time saved
2 min    45 (5 fewer)      5, of 2.0       7 s                     ~0.1 h
5 min    34 (16 fewer)    14, of 2.1      56 s                     ~0.3 h
10 min   29 (21 fewer)    14, of 2.5     133 s                     ~0.4 h
```

Every batched review waits for its window. With a blocking submit, that wait lands on an agent; at
5 minutes it is more agent time (0.8 h) than reviewer time saved (0.3 h). With asynchronous submit,
the wait only delays the land.

### One reviewer, three changes: does attribution hold?

Three small past changes went to Codex in one call, with diffs and a request to name a change on
every finding and give a verdict per change: [NIT-122](nitely://issue/NIT-122) (29 lines in land.rs
and procs.rs, passed), [NIT-84](nitely://issue/NIT-84) (52 lines in land.rs and submit.rs, whose
review had found a P1), and [NIT-43](nitely://issue/NIT-43) (26 lines, passed). Two of them touch
land.rs.

```
                       time    tokens    NIT-122   NIT-84                      NIT-43
three calls (one each) 114 s   108,600   clean     P2 same-second approval    clean
one batched call        99 s    57,500   clean     P0 same-second approval    clean
the original review                                P1 same-second approval
```

- The batched call found the same defect as the original review and as the single call, named the
  right change, and passed the other two. Nothing leaked across changes, although two of them
  touch the same file.
- It used 47% fewer tokens and 13% less reviewer time than three calls.
- Each change waited as long as the slowest: NIT-122's agent would have waited 99 s instead of 21.
- **Severity drifts more than attribution:** the same defect came back P0, P1 and P2 across three
  runs. At P2 the change would have passed. This is one experiment (n = 1), and it says what to
  watch: per-change verdicts depend on per-finding severity, which is the noisiest part of a
  review whether batched or not.

### Land attempts

```
168 land attempts: 121 landed, 31 stopped on a rebase conflict, 15 on a failed gate, 1 other
136 land gates (an attempt that stops on a conflict never reaches its gate)
the land lock held 4.7 h in all, a median of 114 s per attempt
```

A land queue gates the stacked tip of everything waiting once. On a gate failure it bisects. A
conflict is found while stacking, before any gate, and that change goes back to its agent as
today.

```
fixed window   queue runs   gates (today 136)   lock      added wait per land
none (today)   168          137                 4.3 h     0 s
2 min          142          136                 4.3 h     7 s
5 min           85          107  (-21%)         3.4 h     85 s
10 min          56           88  (-35%)         2.8 h     203 s
```

A window buys fewer gates with waiting. A queue that takes only what is already waiting when the
last run ends adds no wait at all. At yesterday's load it formed 3 batches, because the queue was
almost always empty: the median land waited 0 s, the p90 71 s (42 s with 85-s warm-lane gates).

The same arrivals against slower gates show when batching pays:

```
gate     taken at once   gates   wait to land: median   p90
5 min    1 (today)       137     51 min                 133 min
5 min    up to 5         113      3 min                  12 min
10 min   1               137     443 min (never catches up)
10 min   up to 8          82     49 min                 165 min
30 min   up to 8          82     854 min (gating needs 41 h of a 21 h day)
```

With a 5-minute gate, landing one at a time cannot keep up with a busy day; a queue that takes what
is waiting can. With a 30-minute gate neither keeps up, because 15 gate failures in 168 attempts
each cost a bisection: such a project needs fewer failures (flaky retries, NIT-160), faster gates
(lanes, the gate split into what a diff needs), or a CI executor that gates several stacks at once.

In 5-minute land batches, 48 of 110 pairs of different issues touched a file in common. Stacking
does not add conflicts beyond landing them one after another, which rebases each onto the other
anyway: the same 31 conflicts would show up, at the same points.

## Three pipelines, decided separately

| | What batching saves | What it costs | Recommended |
|---|---|---|---|
| **Review** | reviewer startup (~65 s and ~half the tokens per extra change) | the batch waits for its window and its slowest change; noisier severities to watch | `"off"`; `"small"` for projects with many tiny changes, with async submit |
| **Gate** | one gate for the stacked tip instead of one per change | bisection when the tip fails | together with land, below |
| **Land** | one lock hold and one push for several changes | stacking order matters; one bad change costs extra gates | a queue that takes what is waiting (`"waiting"`) |

The gate and land go together: gating a stack that is not then landed as gated proves nothing.

## Review batches

### Forming a batch

A submitted change waits in the review queue when its project batches reviews. A batch closes when
it reaches `max` changes, when its oldest change has waited `window`, or, with `window = "busy"`,
when a reviewer slot frees up (the machine's reviewer slots, as for lanes' build slots, NIT-199).
A batch holds changes from one repository only (a project can span several), so the reviewer works
in one checkout that has every change's commits.

**Small** means at most `small_lines` lines and `small_files` files changed against the base
(default 100 and 5: 15% of this repo's reviews). With `batch = "all"`, any size can join, up to a
total of `max_lines` (default 1,500) per batch, because a reviewer's attention is the limit.

**What never batches** (it is reviewed alone, at once):

- a change that will need a human's approval anyway: the playbook's `[approval]` paths, a flag a
  previous review of it raised, or a project with `require_cross_vendor` whose only reviewer is
  the author's vendor;
- a change to the playbook itself (`.nitely/playbook.toml`) or to the review rules;
- a change whose previous review asked for changes and that still has open P0/P1 findings: it is
  re-checked alone against them (NIT-161's rounds). One with only accepted findings (NIT-191) may
  batch;
- a project in large-repo mode (build-lanes.md), whose reviewer works in the person's own checkout;
- a resubmission of a change already in the queue: it replaces the queued one.

### What the reviewer gets

One prompt, one section per change, in a fixed order:

```
## CHANGE 2: NIT-84 "Keep an approval when only the base moved"
branch claude/nit-84-..., base 3b8179d, head 407083d
Previous review's open findings: 1. [P1] ...        (only for a re-review)
Run `git diff 3b8179d..407083d` ...
```

and the same closing rules as today, per change:

```
- [P1] CHANGE 2: title: file, symbol: what goes wrong and when
PREVIOUS 2.1: fixed
VERDICT 1: clean
VERDICT 2: findings
```

The reviewer is told that each change will land on its own, that a defect belongs to the change
whose diff introduces it, and that a problem that only appears when two changes meet goes on an
`INTERACTION 1,2: ...` line instead.

### Per-change verdicts, attributed

- The parser splits the reply by change. Each change gets its own review row (issue, patch-id,
  commit, findings, verdict), as if it had been reviewed alone, plus the batch's id and a note
  ("batched with NIT-122, NIT-43"). Everything that reads reviews today (land's "review covers this
  patch", NIT-161's carried findings, NIT-191's accepted findings, the PR description) keeps working
  per change.
- A P0 or P1 on change 2 sends back change 2 only; the others go on to land.
- A finding that names no change, or a change the batch does not hold, makes the batch's verdicts
  unreliable: every change it could belong to is reviewed again alone. So is every change whose
  VERDICT line is missing.
- An `INTERACTION` line is not a finding against either change on its own, but it blocks the pair:
  the first of the two may land, and the second is reviewed again alone, on top of the first, before
  it can land. The note goes on both issues.
- A batch whose reviewer fails to run or finish goes back to the queue as single reviews, so no
  batch ever leaves a change without a review.

### How it runs (NIT-216)

- **Only a submit in the background waits for a batch.** `submit` hands the change to a job of its
  own (NIT-212), and that job waits in the queue after its gate passes; nobody is held by the
  wait. `submit` with `wait: true` (`nitely submit --wait`) always reviews alone, at once.
- **No resident process.** Each waiting job polls its row in `review_queue`. The first one to find
  its queue's batch due takes it under the queue's file lock, runs the reviewer once, and writes each
  change's review into its row; each job then goes on with its own change exactly as after a single
  review (recording the review row, the pull request, the issue's move, the land by Nitely).
- **A queue is a repository and a reviewer.** Codex's changes and Claude's are reviewed by
  different reviewers, so they queue apart. `busy` closes a batch as soon as no batch of that queue
  is under review: changes collect only while one is.
- **The reviewer runs in the first change's worktree** and reads the others with `git diff` and
  `git show <head>:<path>`; every worktree of a repository sees every branch's commits. Its tokens
  count toward that first change's issue.
- **An interaction** becomes a P1 finding on the second change ("Interacts with NIT-x, reviewed in
  the same batch: ... (the files both touch). Once NIT-x has landed, rebase onto it and submit
  again"), and a note on both; when the second change's own part of the reply could not be used,
  its single review carries the finding. Its next submit has an open P1, so it is reviewed alone,
  and the finding stays open, whatever that review says, until NIT-x is Done and the branch holds
  the commit it landed at.
- **What waits stops being waited for:** a job that dies while waiting leaves the queue
  (`abandoned`), a batch whose reviewing process dies sends its changes back alone, and a newer
  submit of the issue replaces its waiting change (`superseded`; its job ends as superseded).
- Each review row carries `batch_id`; `review_batch` holds the batch (its issues in prompt order,
  reviewer, seconds, and the error when it failed).

### Severity

Batching does not change how severities are judged, but the experiment shows they drift between runs
(P0, P1 and P2 for one defect). The batched prompt keeps the single prompt's definitions word for
word, and a later slice measures the drift: the same small change reviewed alone and in a batch,
over a day of real submissions, comparing verdicts. Batching stays opt-in until that comparison
says it does not let defects through.

### Measuring it: shadow batches (NIT-217)

`[review] batch = "shadow"` measures batching without using it. Every change is reviewed alone,
and that review decides, as with `"off"`. A small change that could have joined a batch (the same
rules as `"small"`) is then reviewed again in a shadow batch: once its submit job has handed the
result back and released the issue, the job's process joins the shadow queue, and the batch is
reviewed and recorded like a real one, but nothing reads it. No note, event or state comes from it.

`nitely batches compare` sets each shadow batch's review of a change against the single review
that decided it: the same verdict, the same P0/P1 findings (paired by the file and symbol they
cite), findings both raised at a different priority (and how many of those crossed between P1
and P2, which is what changes a verdict), and reviewer seconds and tokens per change (Codex
reports its tokens; a batch's are shared evenly). `--json` gives every pair.

For numbers before a day of traffic, `cargo run -p nitely-core --example review_replay` replays a
project's recent small first-round changes through the reviewer, alone and in batches. It opens
the database read-only and reviews in scratch clones of the repo. It also compares each fresh single
review with the change's original one: how much two reviews of one change disagree anyway, the
baseline batching is judged against.

### What the replay found (2026-10-17)

This repo's 15 most recent small first-round changes (9 to 70 lines, 1 to 5 files) went to Codex,
each alone and then in five batches of three, in the order they were handed in:

```
                                   batched vs alone    alone (fresh) vs the original review
same verdict                       14 of 15            14 of 15
P0/P1 findings                     1 batched, 0 alone  1 original, 0 fresh
same finding, priority crossed P1  1                   1
reviewer seconds per change        33 (a batch: 100)   48
tokens per change                  20,900              36,300 (42% fewer batched)
```

- **The one disagreement is the same defect, and batching was not the side that drifted.**
  [NIT-166](nitely://issue/NIT-166)'s original review raised a P1 (a grid with five columns for a
  row that still renders six). The fresh review alone called the same defect P3, which would have
  passed the change. The batch called it P1, as the original did. Over these 15 changes, a batch
  agreed with the original review exactly as often as a second review alone did.
- **Attribution held.** No finding was left without a change or put on the wrong one, and no
  change went alone after all. The other findings (all P2 and P3) stayed on their own changes.
- **Cost:** 42% fewer tokens and 30% less reviewer time per change. Each change waits for its whole
  batch, though: 100 s on average, against 48 s alone, plus the window.
- **The sample is thin where it matters.** One change in 15 had a blocking finding. This shows
  batching does not drift more than a single review does. It does not show how batches handle a
  run of real defects.

**Recommendation for NIT:** not `"small"` yet. Turn on `"shadow"` for a day of real submissions
(a playbook change, so Andrew approves it), then read `nitely batches compare`. Turn on
`"small"` if, over at least 30 shadowed changes, the batches' verdicts agree with the single
reviews as often as two single reviews agree with each other, and no P0/P1 that a single review
raised is missing from its batch.

## The land queue

When a project batches landing, `land` puts the change in the project's queue instead of landing it
at once. The queue runs under the land lock, one run at a time:

1. **Take** what is waiting, oldest first, up to `max` (default 5). With `window = "waiting"`, only
   what is already queued. With a time window, it waits for the window to close.
2. **Check** each change as land does today: review passed and covers its patch, approval stands,
   nothing changed since review.
3. **Stack**, in order: rebase each change onto the integration branch plus the changes before it,
   in its own worktree (git only). A change that conflicts goes back to its agent (In Progress,
   "conflicts with main or with NIT-x, landing just before it"), and the rest continue. A change
   whose patch-id changed during the rebase is no longer covered by its review: it leaves the
   queue for a re-review.
4. **Gate the tip** once, as a lane request (NIT-198), with the playbook's flaky retries (NIT-160).
5. **Land**: on a pass, fast-forward the integration branch to the tip and push once. Each change
   is landed: its own commits, its issue Done, its PR merged, its worktree cleaned up.
6. **On a failure**, bisect: gate the first half of the stack, land it if it passes, then the rest.
   One bad change in a batch of k costs at most 1 + 2⌈log2 k⌉ gates (3 for 2, 5 for 3 or 4, 7 for
   5 to 8) instead of k. The bad change goes back to its agent with the gate's output. Before
   halving, one cheap guess: when exactly one change touches the files the failing step names (a
   failing test's file, a compile error's path), that change is taken out and the rest are gated.

The person's approvals stay per change: a batch never lands a change that was not approved on its
own, and the app signs each approved patch, as today.

### As built (NIT-215)

`crates/nitely-core/src/land_queue.rs`, with `[land] batch = "waiting"` or `"all"`:

- **Who queues.** `land` puts the change in the repo's queue (`land_queue`, one row per change) when
  the playbook batches, the repo has `[lanes]`, it is not in large-repo mode, and the change needs no
  person's approval (its review's approver is not "human" and it names no approval reasons). Those
  land alone, as before, so approvals are given and signed per change. So does a land that is not
  to be pushed (`--no-push`), since a stack is pushed as one. A land of one change alone that finds
  changes queued behind it when it finishes lands them too, still under its lock, unless it was kept
  local (their push would publish it).
- **Who runs it.** No resident process. Whichever `land` call finds the land lock free runs the
  queue until nothing waits, landing other agents' changes too. The others wait on their own row and
  return once it landed or was sent back, or after 20 minutes saying it is still waiting. Landing
  again then finds the same row, not a second one. A run that died leaves its rows `running`; the
  next run, under the lock, queues them again. A lock whose process is gone (the machine went down
  mid-land) counts as free: the next land takes it over and says so. A git `index.lock` in the main
  checkout from before the land began, left by a git that died mid-command, stops the land before
  its gate with the file's path; land never deletes it, since a git may still be running there.
- **Stacking.** The stack is built in its own worktree, `<lanes dir>/stack`, by rebasing each branch
  head onto the tip before it there. An agent's branch is moved only when its change lands, so a
  change sent back keeps the branch it handed in. A conflict says "conflicts with main or with
  NIT-x, landing just before it". A change whose patch no longer matches its review ("changed since
  its review") goes back for a new submit.
- **The gate** runs once at the tip, as a land request in a build lane, with the playbook's retries.
- **Bisecting.** On a failure, the guess comes first. If exactly one change touches a file the
  failing output names (the echoed `$ command` lines do not count), the others are stacked again
  without it and gated; if that passes, they land and it goes back. Otherwise the run halves, gating
  prefixes: the shortest failing prefix ends with the bad change. The prefix before it lands, the bad
  change goes back with the failing gate's output, and the changes after it wait for the next run.
- **Landing.** The rebased heads are pushed to their PR branches first. Then main is fast-forwarded
  to the tip and pushed once, the `[[land.after]]` steps run once, and each change is landed as land
  lands one: issue Done, branch moved to what landed, worktree cleaned up. If main moved during the
  gate, nothing lands from that stack and its changes wait for the next run. A branch that gained
  commits during the gate goes back (what it holds now was neither reviewed nor gated) and keeps
  them; the others wait for the next run. `--keep` keeps a crew worker's worktree too.
- **Records.** `land_run` holds one row per run: changes in stack order, landed, sent back, gates,
  head. Each change's land result (with `batch`: run, the changes it went with, gates) is on its
  `land_queue` row, and its events are on its issue's timeline.

## Asynchronous submit, weighed against batching

[NIT-212](nitely://issue/NIT-212) turns `submit` into a handover: it records the submission and
returns, the gate and review run in Nitely's background runner, and the result reaches the agent
through `submit_status`, `next_task` or its next nitely call. A crew worker exits after submitting,
and the supervisor starts a worker for the fixes with the handoff.

- It removes the agents' share of the wait for every change: 10.3 h over 389 reviews, plus the gate
  before each review. Batching reviews removes about 1 minute of reviewer time per extra change and
  adds a window's wait.
- It is what makes review batching acceptable: with a blocking submit, a batch window is waiting an
  agent pays for. With async submit, nobody waits on it.
- It does not reduce reviewer calls or tokens. Batching does, by about half per batched change in the
  experiment.

So async submit comes first and is not optional. Review batching is a cost lever for projects that
produce many small changes. The land queue is a throughput lever for projects with slow gates.

Fewer reviews also beat cheaper ones: 103 of the first 344 reviews checked a change whose last review
had already passed (a resubmit after a rebase, a conflict fix or a stopped land), 13 of them with an
identical patch. Reusing a pass for the same patch and reviewing only the difference otherwise
([NIT-185](nitely://issue/NIT-185)) saves more reviewer calls than a 5-minute batch window (16).

## What the person sees

- **Board:** a batch is one row while it runs, "Review batch · 3 changes" or "Landing 3 changes",
  with the issues' keys and its step (reviewing, stacking, gating 2 of 3, bisecting). Each issue keeps
  its own track, and its station says "in a review batch with NIT-122, NIT-43" or "landing, 2nd in the
  queue".
- **Issue timeline:** the batch's review and its verdict for this change, and for a land, the queue
  run with the gate's log; a change sent back from a batch says why ("its own P1", "the stack's gate
  failed and bisection found it", "conflicts with NIT-x").
- **Inbox:** changes that need a person's approval never batch, so they arrive one per decision, as
  today. When several of them wait at once, the Inbox groups them in one card ("3 changes wait for
  your approval"), each with its own Approve and Send back, and an Approve all that signs each patch
  on its own.
- **Overview:** the land queue's length next to the lanes strip (build-lanes.md).

### As built (NIT-218)

- `crates/nitely-core/src/batches.rs` reads it all for the app, from the queues' own rows: the
  review batches under review and the changes waiting for one (`review_batch`, `review_queue`), and
  each land queue with its running run (`land_queue`, `land_run`). The run records its step as it
  goes (`land_run.step`: "stacking", "gating the stack", "bisecting: gating 2 of 3", "landing").
  `batches_overview` is the board's read; an issue's detail carries its own batches.
- Why a change went back is read from what the batch said: a review verdict with its own P0/P1, or
  only an interaction finding (it waits for the other change to land); a land error that bisection
  or the one-file guess found, a conflict with the changes stacked before it, a branch that moved.
- A shadow batch (NIT-217) shows on the board in a dashed, quiet row ("recorded only") and on the
  timeline greyed, "Recorded only; its own review decided". It never sets an issue's station.
- The Inbox groups change approvals only when each one's details say it is a plain approval: a
  playbook change, a goal plan or a mirrored issue keeps its own card. Approve all calls the same
  approve as each card's button, one change after another, so each patch is signed on its own.
- Rules shows the project's `[review] batch` and `[land] batch` settings read-only, and says when a
  land batch cannot run (no `[lanes]`, large-repo mode).

## The playbook

```toml
[review]
batch = "off"          # "off": each change alone. "small": small changes together. "all": any size. "shadow": alone, and measured in batches.
batch_window = "5m"    # how long the oldest change waits for others; "busy": until a reviewer frees up
batch_max = 3          # changes per review call
small_lines = 100      # what counts as small
small_files = 5
max_lines = 1500       # with "all": lines per batch

[land]
batch = "off"          # "off": each change lands alone. "waiting": a queue that takes what waits. "all": a time window too.
batch_window = "0"     # with "all": how long a queue run waits for more
batch_max = 5          # changes per queue run
```

`"off"` is the default for both, and a playbook without these keys behaves as today. A project can
batch landing without batching review, and the reverse.

## GitHub and Bitbucket

**One pull request per issue, as today.** Each PR carries its issue's review, approval and patch
marker (`nitely review patch=...`), and mirrors close their issue from it (mirror.rs). A combined PR
would mix verdicts, and a change sent back from a batch would leave it half done.

Landing a stack pushes the integration branch to the stacked tip, which holds every change's
commits. GitHub and Bitbucket each mark a PR merged when its head's commits reach the base branch,
which is how PRs show merged after a land today. A PR whose change was rebased in the stack is pushed
with its rebased head first (`--force-with-lease`, as submit does), so the host sees the same
commits on main.

The code host's own merge queue (GitHub merge queue, Bitbucket's merge checks) is not used: it needs
branch protection and CI-run gates, and Nitely's gate runs on the person's machine. A CI executor
(NIT-200) could later gate a stack there; it would still be Nitely's queue deciding what goes in it.

## Appended test commands

Land can reconcile a conflict confined to `package.json` when both sides only append distinct,
explicit `.js`, `.mjs`, or `.cjs` file arguments to the same `node --test` command in `scripts.test`.
An unchanged setup prefix such as `tsc -p tests.json &&` is supported. Other JSON changes, changed
commands or options, removed files, quoted arguments, overlapping additions and globs remain
manual conflicts.

After reconciliation, Nitely proves the original reviewed patch added the same test arguments and
made the same changes outside `package.json`. Any required approval must still have a valid app
signature for that original review. This creates neither a new approval nor a synthetic review.
The combined candidate follows the ordinary integration gate policy.

## Build order

1. **Async submit** ([NIT-212](nitely://issue/NIT-212), landed): submit jobs in processes of their
   own, `submit_status`, results through `next_task` and notices.
2. **The land queue** (`[land] batch = "waiting"`): queue table, stacking, one gate per run in a lane,
   bisection, per-change land records. Needs lanes for the stacked tip's gate (NIT-198).
3. **Batched review** (`[review] batch`, [NIT-216](nitely://issue/NIT-216)): the queue, the batched
   prompt, the per-change parser, review rows with a batch id, fallbacks to single reviews.
4. **The board, timeline and Inbox** for batches and the queue.
5. **Measuring it**: severity drift between single and batched reviews over a day; queue waits,
   batch sizes and bisections in the retro's terms.

## How it was measured

On 2026-10-17, read-only:

- **Reviews:** the `review` table (one row per submit; reviewer, verdict, findings, commit) and the
  `review_started` event before each, for the review's duration (dropping reviews over an hour, which
  were interrupted sessions). Diff size: `git merge-base <commit> origin/main` and
  `git diff --shortstat` in this repo, which reads objects only; 58 reviewed commits were no longer in
  the object store. Batches: greedy windows over the small reviews' start times.
- **Lands:** the `Rebasing … onto main` status to `landed` or `land_failed` for each attempt, the
  cause from the stop's summary, and the attempt's lock time. The queue simulation replays the
  arrivals, drops conflicts before the gate, and halves on a failed gate.
- **The experiment:** `codex exec --sandbox read-only` from the main checkout, the three diffs inline
  with their commits for context, once batched and once per change; tokens from Codex's own count.
  The scripts and replies are not kept.
