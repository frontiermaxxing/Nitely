# Crew: Nitely runs the agents

**Status:** design for [NIT-96](nitely://issue/NIT-96), 2026-10-16. Built in three slices: the
supervisor and `nitely crew run`; crew markers and the Project header control; the guardrails.

Today an agent works on a project only while someone keeps a chat session open and tells it to call
`next_task`. A crew is Nitely doing that itself: it keeps up to N headless agent processes working
on a project's backlog, one issue each, and starts the next when one finishes. Everything a worker
does goes through the same tools a person's agent uses (`start_task`, `report`, `submit`, `land`),
so the board, the review and the gate treat it like any other agent.

Nothing runs until a person turns it on: the default is `workers = 0`.

## The playbook's `[crew]` section

```toml
[crew]
workers = 2                   # how many run at once; 0 (the default) is off
agents = ["claude", "codex"]  # preference order; agents that are not installed are skipped
max_per_day = 20              # workers started per project per local day (default 20)
max_tokens_per_day = 5000000  # optional: no new worker once the project used this many tokens today
quiet_hours = "01:00-07:00"   # local time; no new worker starts inside it (running ones finish)
timeout_minutes = 180         # a worker running longer is stopped (never while it lands)
setup_timeout_minutes = 30    # a worktree setup running longer is stopped; its issue goes to a person
attempts = 3                  # failed workers on one issue before it goes to a human

[crew.commands]               # optional: replaces the default command for an agent
codex = ["codex", "exec", "--sandbox", "danger-full-access", "..."]
```

The section lives in the repo's playbook, so a crew belongs to a repo and works on that repo's
project. The playbook is reread on every tick: lowering `workers` drains the crew down to the new
number (nobody is stopped), and `workers = 0` drains it to nothing.

The crew never starts when the playbook has no gate (`[gate]` empty): an unattended worker with no
gate could land anything that builds. `nitely crew run` says why and waits.

### The token budget

`max_tokens_per_day` counts every agent working in the project's repo today, crew workers and
people's own sessions alike: new input, cache writes and output (cache reads are cheap and counted
apart). Nitely reads the counts from the agents' own session files (`crates/nitely-core/src/usage.rs`):
Claude Code's transcripts and Codex's session logs, counts and model names only, never the text.
Once the budget is used, running workers finish and no new one starts until the next local day;
the project header says `token budget used: 5.2M of 5.0M today`. It is in tokens, not money: plans
price tokens differently, and a wrong amount is worse than none. `nitely spend` shows an issue's or
a project's spend; the project summary shows tokens per day and the issue panel one line per issue.

## The supervisor: `nitely crew run`

```
nitely crew run [--project NIT]     run the crew for a project, in the foreground
nitely crew status [--project NIT]  workers running, cap, paused, today's count, last failures
nitely crew pause | resume [--project NIT]
```

**Nobody has to run it.** While the app runs, it checks every 20 seconds that each project whose
playbook has `workers > 0` has a supervisor with a fresh heartbeat, and when one has none it starts
`nitely crew run --managed` (the `nitely` on PATH) apart from itself: no window, its own process
group, output to `supervisor.log` in the project's crew folder. It outlives the app, so closing the
app does not stop the crew; pause it, or set `workers = 0`, for that. A managed supervisor leaves by
itself when the crew is off and idle. When `nitely install` puts a newer build in place, it starts
that build to take the crew over and leaves once the new one holds it (its workers keep running,
and the new one adopts them), with or without the app open; if the new build does not take over,
the old one carries on. If the app cannot
start one, the crew's status says why.

`crew run` is a loop that ticks every 10 seconds:

1. **Reap.** For each worker it started, check whether the process exited; if so, settle it (see
   Guardrails) and record the outcome.
2. **Decide.** Read the playbook and the crew's pause flag. A slot is free when fewer than `workers`
   are running, the crew is not paused, it is outside quiet hours, today's count is under
   `max_per_day`, the project's tokens today are under `max_tokens_per_day` (when set), and the
   gate exists.
3. **Claim.** For a free slot the supervisor claims the next issue itself (`next_task`'s query) and
   runs `nitely start <key> --agent <vendor>` for it in a process of its own (`start_task`: branch,
   worktree, the playbook's setup), so the worktree exists before the agent starts and the
   supervisor knows which issue each worker holds. The setup holds the slot but never the
   supervisor: each tick looks whether it finished, a failed one sends the issue to Needs You with the
   end of its log, and one past `setup_timeout_minutes` is stopped and does the same. It is recorded
   (`crew_worker.phase = setup`), so a new supervisor watches it, or starts it again when it is gone. The claim is recorded under the worker's agent id
   (`claude:<worktree>`), the same id the worker's MCP server will use, so the worker's own `report`
   and `note` calls keep the claim fresh. Nothing ready: the slot stays empty until the next tick.
4. **Start.** Launch the agent in the worktree with the brief (below) on stdin.

Which issue: `next_task` keeps parallel workers out of each other's files. Every issue in flight
holds the files its branch changes and the paths its body names; a ready issue whose title, body or
labels name one of them collides, and a clear issue at most one urgency level below is taken instead
(an urgent one never waits). Files nearly every change touches go in `crew.shared_files` and count for
a quarter. Whenever in-flight work changed the pick, the result says why (`NIT-40 holds src/Inbox.tsx`),
and the crew notes it on the issue. The branch map shows what each issue holds on hover.

Which agent: the first in `agents` that is installed and not failing to start. An issue that already
has a branch from an earlier attempt keeps that branch's vendor when it is in the list, so a second
attempt continues the first one's work instead of starting a new branch beside it.

One supervisor per project: `crew run` writes a heartbeat row and refuses to start while another
live supervisor's heartbeat is fresh (under a minute old and its pid alive). The app reads the same
row to say whether the crew is actually running.

### What is stored

- `crew_worker`: one row per worker: project, issue, vendor, pid, worktree, log path, started and
  ended time, exit code, outcome (`submitted`, `landed`, `asked`, `failed`, `launch_failed`,
  `stopped`) and a short reason.
- `crew_control`: per project: paused or not, who changed it and when; the supervisor's pid and
  heartbeat.
- Timeline events on the issue: `crew_started` ("Crew worker started (codex), log: <path>") and
  `crew_ended` with the outcome. The log path is in the event, so the issue panel links it.

### Logs

Each worker writes stdout and stderr straight to its own file,
`<data dir>/crew/<PROJECT>/<started>-<key>-<vendor>.log` (`%APPDATA%\Nitely` on Windows,
`~/Library/Application Support/Nitely` on Mac). Files, not pipes: a worker must not die because the
supervisor did. The logs hold the agent's full output, so they stay in the data dir on this machine
and never go into the database. The supervisor keeps the last 200 per project and deletes older ones.

## The worker

### How each agent is started

Both get the brief on stdin, run with the worktree as their working directory, and get the `nitely`
MCP server explicitly, so a worker never depends on a project MCP file having been approved. Session
markers the supervisor itself might carry (`CLAUDECODE`, `CLAUDE_CODE_*`, as `review.rs` strips) are
removed from the worker's environment, and `NITELY_CREW=<worker id>` is added.

**Claude Code** (`claude` 2.1):

```
claude -p --mcp-config <data dir>/crew/mcp.json
          --permission-mode auto --permission-prompts none
          --output-format stream-json --verbose
```

`mcp.json` is written by the supervisor: `{"mcpServers":{"nitely":{"command":"<this nitely>","args":["mcp"]}}}`.
`--permission-mode auto` lets the agent edit, build and commit without a person; `--permission-prompts
none` turns anything that would have asked into a refusal instead of a process waiting forever for an
answer nobody gives. The user's hooks still load, so the worker shows on Now through them.

**Codex** (`codex exec` 0.163):

```
codex exec --sandbox danger-full-access
           -c approval_policy="never"
           -c mcp_servers.nitely.command="<this nitely>" -c mcp_servers.nitely.args=["mcp"]
           -c mcp_servers.nitely.default_tools_approval_mode="approve"
           -
```

Checked on this machine: without `default_tools_approval_mode="approve"`, `codex exec` refuses every
MCP call ("requires approval, but approval policy is never"); with it, the nitely tools work.

No sandbox, on purpose. The live check ([NIT-118](nitely://issue/NIT-118)) found that codex's
`workspace-write` sandbox never lets git write under a `.git` folder, whatever `--add-dir` or
`sandbox_workspace_write.writable_roots` say, and a worktree's index and commits live in the main
checkout's `.git/worktrees/`: a sandboxed worker made its change and then could not `git add` it. So a
codex worker runs with the trust Claude's `--permission-mode auto` gives a claude worker, which is
also no OS sandbox. On Windows the sandbox also cannot enter folders under the user profile's
`AppData`, so a repo kept there could not be worked in at all. Codex is found in the Store app's
folder on Windows, as the reviewer is.

Either command can be replaced per agent with `[crew.commands]`, e.g. to pick a model or loosen the
sandbox. `{worktree}`, `{repo}` and `{mcp_config}` are filled in.

**Before turning a crew on:** each agent's CLI must be signed in on its own. The Claude desktop app
signs in its sessions itself, so a plain `claude -p` can fail with "OAuth session expired" while the
desktop app works; run `claude` in a terminal once and log in. The supervisor recognises a worker
that fails to start (below) and says so instead of burning issues.

### The brief

The same text for every agent, with the blanks filled in:

> You are a Nitely crew worker: Nitely started you to work on one issue, {key}, "{title}", in project
> {project}. Nobody is watching this session; nobody will answer a question here.
>
> The issue is claimed for you and its worktree is ready: {worktree} (branch {branch}). It is your
> working directory. Work only there; never edit or commit in {repo}.
>
> 1. Read the issue with get_issue {key}, and the repo's AGENTS.md.
> 2. Do the work. Call report with key {key} at each step, one line.
> 3. File anything you find but will not fix with file_issue.
> 4. If only the human can decide something, ask it, move {key} to Needs You, leave a handoff note
>    and stop: do not wait for the answer.
> 5. Commit, then call submit. If the review sends it back (P0 or P1 findings), fix them, commit and
>    submit again, up to three rounds; then file what is left and stop.
> 6. When submit says it passed and no human approval is needed, call land. On a rebase conflict,
>    resolve it in the worktree, then submit and land again. If land says it could not remove the
>    worktree because a process still uses it, that process is you: Nitely removes it after you exit,
>    so do not file it.
> 7. Before you stop, for any reason, leave a note on {key} with handoff: true: what is done, what
>    is left, what you were unsure about.
>
> Then end. Do not take another issue: Nitely starts a new worker for it.

## Stop, pause, resume

- **Pause** (app or `nitely crew pause`): no new workers start; running ones finish their issue.
  **Resume** lifts it. Pause is in the database, not the playbook, so it is per machine and does not
  need a commit.
- **Ctrl+C on `crew run`** once: the supervisor stops starting workers and waits for the running
  ones to finish, printing who is still working. Again: it stops every worker that is not landing,
  settles their issues as `stopped` (handoff note from the last report, back to Todo, not counted as
  a failed attempt), waits for any landing one, and exits.
- **Never mid-land.** A worker is landing while the repo's land lock (`nitely-land.lock` in the git
  common dir) names its issue. Neither the second Ctrl+C nor the timeout stops it then; the
  supervisor waits for the lock to be released.
- **The supervisor dies** (crash, closed terminal, reboot of the terminal app): its workers keep
  running, since nothing ties them to it. The next `crew run` reads `crew_worker` rows with no end,
  adopts those whose pid is still alive (counting them toward the cap and settling them when they
  exit, from the issue's state since the exit code is gone), and settles the dead ones at once.

### Processes on Windows

- Workers start with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`: no console window, and Ctrl+C
  in the supervisor's console does not reach them (a console sends Ctrl+C to every process attached
  to it, which would kill workers mid-land).
- No job object with kill-on-close: a supervisor that dies must not take its workers with it.
- Stopping a worker kills its tree (`taskkill /T /F /PID`), since `claude` and `codex` start shells,
  builds and MCP servers under them.
- `crew run` asks Windows not to sleep while a worker runs (`SetThreadExecutionState` with
  `ES_SYSTEM_REQUIRED`), and lets go when the crew is idle. The display may still turn off.

### Processes on Mac

- Workers start in their own process group (`process_group(0)`), so the terminal's SIGINT reaches
  only the supervisor. Stopping a worker sends SIGTERM to its group, then SIGKILL after 10 seconds.
- Sleep: `crew run` holds a `caffeinate -i -w <pid>` while workers run.
- Running without the app open at all (a login item through launchd, a scheduled task on Windows) is
  a follow-up; the supervisor itself needs nothing platform-specific for it.

## Guardrails

When a worker exits, the supervisor reads its issue:

| Issue after the worker | Outcome | What the supervisor does |
| --- | --- | --- |
| Done | `landed` | removes the worktree land kept whole while the worker ran in it, the way land would: teardown, `git worktree remove` without force, `git branch -d` |
| In Review, or Needs You after submit | `submitted` | nothing; a person approves, then land |
| Needs You with an open ask from the worker | `asked` | nothing |
| In Progress (or Todo), still claimed by the worker | `failed` | writes the handoff note if the worker left none since it started, from its last report, exit code and log path; clears the claim; moves the issue back to Todo |
| Anything else (a person moved it) | `stopped` | nothing |

- **Retry cap.** When an issue reaches `attempts` (3) `failed` workers, it moves to Needs You with an
  ask instead of back to Todo: "The crew tried {key} three times without submitting. Last: {reason}.
  Logs: {paths}." with options "Try again" and "I'll take it". Answering "Try again" puts it back
  in Todo for a new worker. Only failures since a person last moved the issue or answered the crew
  about it count, so moving it back to Todo by hand works too.
- **Launch failures.** A worker that exits within 2 minutes with a non-zero code and without one
  report is a `launch_failed`: the agent itself is not working (signed out, not installed, a bad
  command). The issue goes back to Todo without counting an attempt. After 2 launch failures in a
  row for an agent, the supervisor stops using it and says why in `crew status` and the app; with no
  agent left, the crew pauses itself with that reason.
- **Timeout.** A worker past `timeout_minutes` is stopped (unless landing) and settled as `failed`.
- **Setup failures.** When `start_task`'s setup fails for an issue, it goes to Needs You with the
  failing command's output in a handoff note: starting it again would fail the same way every tick.
- **An agent that will not start at all** (the program is missing, or the process cannot be
  spawned) is dropped for the rest of the run, the issue goes back to Todo, and the status says
  "no agent will start (claude: ...)" once none is left.
- **No gate, no crew** (above). **Daily cap and quiet hours** only stop new starts.

## What the app shows

- **Project header:** when the playbook has a crew, a control: "Crew 2/3" (running / cap), a pause
  or resume button, and the state in words when it is not working: "paused", "quiet hours", "20 of
  20 today", "no gate", "claude can't start: <reason>", or "not running: start `nitely crew run`"
  when no supervisor heartbeat is fresh. A playbook with `workers = 0` shows nothing.
- **Now and Pulse:** crew workers are ordinary sessions there (their hooks and reports already put
  them on the board), with "· crew" after the agent's name. A session is a crew session when it runs
  in a crew worker's worktree from the time that worker started, so the hooks need nothing new.
- **Issue timeline:** `crew_started` and `crew_ended` entries with the outcome, and "Open log" under
  each, which opens the worker's log (the app opens only `.log` files in its crew folder).
- **Inbox:** the retry-cap asks arrive like any other ask.

## Later

- A login item on Mac / a scheduled task on Windows, so a crew starts without the app being opened.
- One supervisor for every project with a crew.
- Spend: per-worker token and cost totals from the logs, shown on Pulse; a cost cap per day.
- Picking the agent per issue (labels, kind) instead of one preference order.
