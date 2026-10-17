# Build lanes

How one machine runs dozens of agents across several projects: agents edit in light worktrees, and
builds and tests run as requests in a few warm checkouts per project, the build lanes. This is the
design for goal 3, "Many agents per machine", measured before anything in the product changes.
Back to the [README](../README.md); the crew that starts agents is in [crew.md](crew.md).

The constraints: Windows, Mac and Linux alike; nothing to set up (no Dev Drive, no admin rights, no
filesystem drivers); plain git and ordinary files; and when something goes wrong, the worst case is
slower, never corrupted work.

## Why

Today every issue gets a full worktree with its own setup and build output. On 2026-10-17, seven
agents on one PC filled C: by about 16 GB every 40 minutes, Defender held 4 cores, and the disk sat
at 99% busy. The task worktrees on that disk were 6.5 to 16 GB each. Almost all of it is build output
that every worktree builds again from nothing.

## What it costs today, and with lanes

Measured on this repo on 2026-10-17 (32 logical cores, 64 GB, NVMe), on an idle second disk so the
other lanes building on C: did not skew it, with cargo held to 8 jobs. The scripts are not kept; the
method is at the end.

| | Today: a new worktree | A light worktree | A warm lane |
|---|---|---|---|
| Disk per agent | 6.5 GB, 9,775 files | 7 MB, 288 files (sparse: 1.7 MB, 72 files) | none |
| Disk per project | agents × 6.5 to 16 GB | | 1 to 2 lanes × 8.3 GB |
| Ready to edit | after setup | 0.2 s | |
| First passing gate | 244 s (npm ci 2 s, cargo test 185 s, clippy 50 s, npm test 2 s, build 5 s) | | 85.5 s (cargo test 72 s, clippy 6 s, npm test 2 s, build 5 s) |

A warm lane, one request after another (seconds; compile is `cargo test -p nitely-core --no-run`,
run is the test suite itself):

```
request              files switched  switch  compile  test run  npm test  cli build
the same snapshot    0               0.1     0.4      42-45     2         0.4-3.4
a nitely-core diff   2               0.1     5.0      63        2.5       6.6
a frontend diff      6               0.1     5.6      55        2.3       9.1
the two alternating  6 each          0.1     5.8-6.6  50-53     2         5.4-6.4
one line in lib.rs   5               0.1     3.6      42        2.1       5.4
```

What the numbers say:

- Disk per agent drops from gigabytes to megabytes. A project's disk is bounded by its lanes, not
  its agents: 20 agents on this repo would be about 20 × 7 MB plus two lanes of 8.3 GB, against 130
  to 320 GB today.
- A warm lane makes a gate about 3 times faster (85 s against 244 s). Nearly all of what is left is
  this repo's own test suite (42 to 63 s), not compiling.
- Thrash between unrelated diffs is real but small here: switching from one snapshot to another
  rewrites the files both touched, so the crate they live in compiles again, about 5 s. A frontend
  diff after a core diff still recompiles nitely-core, because the switch put the core files back.
  Switching changed at most 6 files and took 0.1 to 0.6 s.
- Touching `lib.rs` costs what any other file in the crate costs: cargo rebuilds per crate.
- A lane's `target/` grows as it alternates (6.4 GB after the cold gate, 8.3 GB after 17 requests,
  2.3 GB of it incremental caches), so lanes need a size cap (below).

For a browser-engine-sized C++ repo on this machine (measured read-only from git's objects, never
touching that checkout): 503,779 files and 4.35 GB of source in its main repository, plus separately
fetched dependencies. Its build output is 20 to 55 GB per configuration. A cone-mode sparse worktree
for a typical feature directory is 742 to 1,621 files and 9 to 21 MB (0.2 to 0.5% of the source); a
whole UI subtree is 14,876 files and 127 MB. One warm checkout per machine is all such a repo can
afford, and light worktrees make that enough.

## The model

**Edit worktrees.** An agent's worktree holds source only: `git worktree add` with no setup that
builds or installs anything, sharing the repo's `.git`. A playbook can make them sparse (cone mode,
`git sparse-checkout`), seeded with the directories the issue names and widened on demand. For
reading code outside its sparse set, an agent has the **reference tree**: one full source-only
checkout of the project's base commit, read-only by convention, moved with the base.

**Build lanes.** A lane is a full checkout that keeps its build output: one or two per project,
`<repo>-lanes/lane-<n>` beside the repo (`[lanes] dir`, like `[worktree] dir`), with NIT-196's
reference tree at `<repo>-lanes/ref`. Only one request runs in a lane at a time, under an OS file
lock (`lane-<n>.lock`) that a crashed process lets go of.

**Requests.** A request is (snapshot, command). The snapshot is a commit in the repo's own object
store: for committed work the branch head; for uncommitted work, a commit Nitely builds from the
agent's worktree in a scratch index (`git status` lists what changed, honoring sparse-checkout; the
changes are added to an index read from `HEAD`; `write-tree` and `commit-tree` make the commit),
without touching the agent's files, index or branch. The commit is kept alive by
`refs/nitely/runs/<id>` until the run's record expires.

A lane serves a request in four steps, the same whether or not the last one finished:

1. `git checkout --force --detach <snapshot>`: only files that differ from what the lane holds are
   written.
2. `git clean -ffdx` keeping only the playbook's build directories (`target`, `node_modules`, ...):
   anything else untracked or ignored, which a previous request may have left, goes.
3. The playbook's setup again when a dependency file differs from the last setup (the fingerprint
   worktrees already keep, `setup_when_changed`).
4. The command, with a timeout, its output in the run's log.

There is no "base plus diff" to apply locally, so there is no failed apply: a local lane and the
agent's worktree share the objects, and a forced checkout cannot conflict. Base plus diff
(`git diff --binary`) is only how a request travels to another machine or to CI.

**Machine scheduler.** One budget for the whole machine across all projects: build slots from cores
and memory (default: one slot per 8 logical cores and 8 GB, at least 1; settable in the app), and
each request gets the slot's share of cores (`{jobs}` in a command, `CARGO_BUILD_JOBS`,
`MAKEFLAGS`). Order: the person's own requests, then land, then submit's gate, then agents' `run`,
first come first served within each, round-robin across projects.

**Executors.** Where a request runs, per project in the playbook: a local lane (the default),
another of the person's machines over the existing relay (`nitely serve`), or CI, for the gate only,
by pushing `nitely/run/<id>` and reading the code host's checks for it.

**Large-repo mode.** For a repo too big for a second checkout, the lane is the person's own main
checkout. Nitely runs a request there only while it is clean (no changes to tracked files, nothing
untracked outside the build directories) and on the integration branch, under the lane lock, and
the app says what is building. After the request it checks out the integration branch again. The
person can also edit in a light worktree like an agent, and then the main checkout is only a lane.

**Copy-on-write.** Where the filesystem clones files for free (APFS `clonefile`, btrfs or XFS
reflink, ReFS block clone), a second lane starts as a clone of the first, build output included.
Elsewhere it starts cold. Clones are an optimization, never required: this PC's NTFS has none, and
every number above is without them.

## Decisions

### The request and its record

```json
{ "id": 42, "project": "NIT", "repo": "C:/Code/nitely", "issue": "NIT-12", "by": "codex:c:/.../nit-12",
  "kind": "run | gate | land", "snapshot": "<sha>", "tree": "<tree sha>", "base": "<sha>",
  "command": "cargo test -p nitely-core", "state": "queued | running | done | lane_error | abandoned",
  "lane": 1, "executor": "local", "queued_at": 0, "started_at": 0, "ended_at": 0,
  "exit_code": 0, "timed_out": false, "setup_ran": false, "files_switched": 2,
  "log": "<data>/lanes/NIT/42.log", "output": "<the end of it>", "error": null }
```

This is `lanes::Run`, the `lane_run` row. `exit_code` is the command's (none when it was stopped
at the timeout); `lane_error` means the lane could not run it at all (`error` says why), which is
never a verdict on the code. A queued or running row whose process is gone reads as `abandoned`.
Logs live in the data folder (`lanes/<project>/<id>.log`; logs and snapshot refs are kept 7 days),
and each run with an issue goes on its timeline as `lane_run`.

A gate request whose tree and gate commands already passed is answered from its record. The retro
found 31 of 418 gate passes repeated a commit that had passed already, and a land whose rebase
changed nothing is one of them.

### `run`

- MCP `run { command, key?, worktree? }`. It snapshots the caller's worktree, waits for a free lane
  and for the command (bounded by `[lanes] timeout_minutes`), and returns the record. A call can be
  forwarded to a newer build in a process of its own, so the wait happens in the call, not in a
  thread that could die with it. `run_status { id?, wait_seconds? }` reads a run (and waits up to
  600 s for an open one); without an id, the caller's last runs.
- CLI `nitely run [--issue] -- <words>` from a worktree (each word passed on as one word), or
  `nitely run --shell "<line>"` for a line meant for the lane's shell, printing the log as it grows
  and exiting with the command's exit code. `nitely lanes [status]` shows the lanes, the queue and the last
  runs; `nitely lanes reset` puts every free lane back to a clean checkout of what it holds (its
  build output stays); `nitely lanes show <id>` prints a run and its log.
- `step` names a playbook gate command, so an agent can ask for "the gate's tests" without spelling
  the command out.
- Commands run with the agent's own trust: an agent that could run them in its worktree can run them
  in a lane. Lanes belong to one person, as the agents do.

### The gate and land

`submit` and `land` keep their meaning. Their gate becomes a lane request (kind `gate` or `land`)
for the branch head, not a run in the agent's worktree, so no edit worktree ever builds. Land's
rebase still happens in the agent's worktree (git only), then the rebased head's gate runs in a
lane, all while land holds its lock. `land` asks for the highest priority, so the land lock is held
for one lane run, not a cold build. `[gate] retries` (NIT-160) applies in lanes as before.

### When a request cannot run

- The snapshot is gone (expired, or the repo was cleaned): the requester gets "snapshot missing",
  not a failure of their code, and `run` makes a new snapshot.
- Switching fails (on Windows, a file the last request's leftover process still holds): the lane
  stops what that request started, by process tree, and tries once more. Then the lane marks itself
  unhealthy and the request goes to another lane, or waits, reported as a lane problem, not a test
  failure. An unhealthy lane is rebuilt from a fresh checkout in the background (its build output
  kept when it can be).
- A lane left mid-request by a crash needs nothing special: every request starts with the forced
  checkout and clean.
- Remote executors: a diff that does not apply on the base there, or a base the remote cannot
  fetch, falls back to a local lane.

### The base commit

Each project has one base commit that agents' worktrees are cut from, lanes rest at between
requests, and the reference tree shows. By default it is the integration branch's tip, moved on
every land: switching is cheap for a repo like this one. A playbook can move it on a schedule
instead (`base = "every 4h"`) for repos where moving costs a dependency sync, so syncs happen a few
times a day, not per agent. Agents still rebase onto the real tip at land; the base only decides
how far snapshots are from what the lanes have built.

### Steering agents to `run`

- Instructions: a light worktree's session instructions say that builds and tests go through `run`,
  and why (the worktree has no build output on purpose).
- A hook nudges: the playbook lists heavy commands (`[lanes] heavy = ["^cargo (build|test|clippy)",
  "^npm (ci|install|test|run build)"]`). A PreToolUse hook that sees one run in a light worktree
  answers with a reminder to use `run`. Hooks never block, so the command still runs. The worst case
  is today's behavior: slower, and the build output lands in that worktree. The cleanup below reclaims
  it.
- The crew's brief says the same, and crew workers always start in light worktrees.

### What each OS gets

The same design everywhere; only lane creation differs.

- **Windows (NTFS):** lanes start cold; light worktrees do the rest (288 files to scan instead of
  9,775 per issue). Defender exclusions help but need admin, so they are only advice in the docs.
  ReFS block cloning is used where a lane sits on a ReFS volume.
- **macOS (APFS):** a second lane is a `clonefile` copy of the first, build output included, in
  seconds and at almost no disk.
- **Linux:** `cp --reflink=auto` on btrfs or XFS, a cold start elsewhere.

### What the app shows

- **Now** and **Overview:** a lanes strip, one chip per lane: its project, idle or what it is
  running (command, issue, for how long), and the queue length, with the machine's slots ("builds 3
  of 4").
- **Issue timeline:** each run with command, exit, seconds and "Open log", like crew logs.
- **Settings** (NIT-168): the machine budget and each project's lanes (count, size cap, executor,
  base policy). Large-repo mode says, in the project header, when the person's own checkout is
  building.

### Moving over

Nothing changes until a project's playbook asks for it (`[lanes]`). Per project:

1. The playbook gets `[lanes]` (count, keep, heavy, executor, base) and `[worktree] mode =
   "light"`. Without them, worktrees and gates work as today.
2. New issues start light. Existing worktrees keep working as full worktrees until they land.
3. `nitely lanes slim` deletes the build directories (the playbook's `keep` list) of worktrees that
   have none of the agent's work in them, after asking, to reclaim the disk at once.

### Lane hygiene

A lane over its size cap (default 2 × its first build) has its incremental caches
(`target/*/incremental`) emptied between requests. Over twice the cap, its build directories are
removed and it builds cold once. Nothing a lane holds is anyone's work: a lane can be deleted at any
moment and rebuilt.

## The playbook

```toml
[worktree]
mode = "light"                       # "full" (today) or "light"
sparse = ["crates/nitely-core"]      # optional: cone-mode directories every worktree starts with

[lanes]                              # no [lanes]: no lanes, everything works as before
count = 2                            # lanes for this project on this machine
keep = ["target", "node_modules"]    # build directories a lane keeps between requests
dir = "../{repo}-lanes"              # where lanes go, relative to the repo root
timeout_minutes = 30                 # a command running longer is stopped with what it started
heavy = ["^cargo (build|test|clippy)", "^npm (ci|install|test|run build)"]
executor = "local"                   # "local", "machine:<name>" or "ci" (the gate only)
base = "tip"                         # or "every 4h"
large_repo = false                   # true: the main checkout is the only lane
```

## Risks

| Risk | What it costs | Mitigation |
|---|---|---|
| Thrash between unrelated diffs | ~5 s here per switch; a big crate or a C++ target with many dependents pays more | requests from the same issue go to the lane that ran its last one; a shared base keeps snapshots close; a second lane per project when the queue grows |
| Latency: requests queue behind each other | one lane serves about 40 full gates an hour here (85 s each); the retro's busiest day was about 21 an hour | priorities (land first), gates answered from records, two lanes, remote executors |
| Agents build locally out of habit | today's cost for that worktree | instructions, the nudging hook, `nitely lanes slim` |
| A false pass from leftovers in a lane | a change lands that a clean build would reject | the clean keeps only the named build directories; land can ask for a clean lane (`[lanes] land_clean = true`); CI as the executor for land |
| A long test suite dominates | lanes cannot speed this repo's 42 to 63 s suite | run only what a diff needs (`step`, `-p`), and test the suite's own slowness as its own issue |
| Large-repo mode touches the person's checkout | the person's work disturbed | only clean and on the integration branch, under a lock; it never commits, never stashes, never deletes untracked files outside the build directories |

## How it was measured

On 2026-10-17, base `56f6f45` of this repo, on an idle NVMe disk, cargo at 8 jobs:

- (a) `git worktree add`, `npm ci`, then the gate's four commands, timed, and `find`/`du` after each.
- (b) `git worktree add` with no setup; and `--no-checkout`, `sparse-checkout set --cone
  crates/nitely-core`, checkout.
- (c) the cold worktree after its gate as the lane. Three snapshot commits: the revert of a two-file
  nitely-core change, the revert of a four-file frontend change, and a one-line edit at the top of
  `crates/nitely-core/src/lib.rs`. Each request did the forced checkout and clean, then
  `cargo test -p nitely-core --no-run`, the test run, `npm test` and `cargo build -p nitely-cli`. A
  first pass without `--no-run` showed the suite's run time dominating; the second pass timed compile
  and run apart. Then one full gate as a single request.
- (d) `git ls-tree -r -l HEAD` in the large checkout's main repository, which reads objects only;
  cone-mode sets computed from that listing; build output sizes with `du`, which only reads.
