# The coordinator, and what replaces it

On 2026-10-16 and 17, one Claude chat coordinated Nitely's own development. It planned the work into
lanes, briefed seven worker sessions, carried the person's decisions to them, recovered them after
a crash and decided what was worth the person's time. Goal 4, "Nitely runs the team", is to do all
of that without the chat: the person writes a goal sentence, approves a plan, and finds landed work
and real decisions in the morning.

This document lists what the coordinator did, with numbers from Nitely's database and the
sessions' transcripts, and decides where each job lives in Nitely or why it stays the person's. The
issues of goal 4 build on it: [NIT-243](nitely://issue/NIT-243) (Crew works a goal),
[NIT-244](nitely://issue/NIT-244) (the planner), [NIT-245](nitely://issue/NIT-245) (triage),
[NIT-246](nitely://issue/NIT-246) (self-recovery) and [NIT-247](nitely://issue/NIT-247) (the goal
view). Back to the [README](../README.md); the crew this extends is [crew.md](crew.md).

## The two days in numbers

From 2026-10-16 00:00 to 2026-10-18 00:00 local time (UTC−5). The method is at the end.

```
the board         256 issues filed, 186 landed (187 lands), 54 lands stopped, 477 reviews (321 passed)
the coordinator   7 worker sessions started, 96 messages to them, 30 messages back from them
                  64 issue updates (22 of them reserving an issue: moved to In Progress
                  before briefing), 23 issues filed, 2 goals proposed
                  woken 36 times by a worker that finished a turn
the person        about 20 messages to the coordinator, 18 asks in the Inbox (12 answered),
                  14 issue moves and 12 answers in the app
```

The coordinator's 96 messages, by what they did:

```
new work for a worker (a lane brief, or the next issue)  39
the person's decision, carried to the worker that needs it  22
answers and judgment calls (resubmit or wait, what to file)  16
recovery after the crash at 23:27 on 10-16            8
duplicate or misrouted work set right                 6
holds and priority changes (disk, CPU, design first)  5
```

The 18 asks the person got:

```
real decisions (license, legal name, outside PRs, icons, crew daily cap,
  approval key location, code signing, keychain migration, a data import,
  a background service)                                              10
things only the person can do (sign a CLI in, grant Git access)       2
noise: findings the code no longer had (4), rounds where the agent
  was right and had fixed it (2)                                      6
```

A third of what reached the person's Inbox was noise, and the coordinator filtered more of it
before it got there: twice it wrote "Don't route this through Andrew" or "No need to involve
Andrew", and once it read the code to confirm a finding was stale (lanes.rs:78 on 2463af7).

## The jobs, and where each one goes

| Job | What it looked like | Where it lives | Issue |
| --- | --- | --- | --- |
| **Plan** | Seven lanes by area (review flow, MCP tools, public repo, Inbox, Crew, bug sweep, onboarding), each an ordered list of 4 to 9 issues; later, lanes picked by who built the code nearby ("you built the brief, so Goals is yours") | The planner: a crew worker turns a goal sentence into an ordered plan of issues, each naming the files it touches, which the person approves once | NIT-244 |
| **Reserve** | 22 issues moved to In Progress "already reserved for you" before the brief went out | `next_task` and the supervisor's claim do it already, atomically | done |
| **Brief** | Each message gave the why, the approach, what to read first and the constraints ("design first, land the doc as its own change") | The issue body (written by the planner) plus the crew brief's fixed rules; nothing per worker | NIT-244 |
| **Keep workers apart** | Lanes by area; file ownership said in notes ("I'm not editing crew.rs"); agreements between lanes through issue notes | `next_task` holds back an issue whose files overlap work in flight (NIT-139, from branch diffs and paths the body names); the planner names each issue's files so overlap sees them before any branch exists | NIT-243, NIT-244 |
| **Start the next thing** | 39 messages, most of them when a lane finished ("Next lane for you"); 36 wake-ups to notice | The supervisor fills free slots with the next ready issue of the active goal, in plan order | NIT-243 |
| **Watch** | Read worker reports, `list_events`, CI runs (`gh run list`) | The goal view and the morning report: landed / total, at risk, expected finish | NIT-247 |
| **Answer routine questions** | 11 of the 30 messages back were questions: who owns NIT-182, resubmit or wait, can an agent unpark a human-owned issue | Triage: an agent's routine question goes to a triage worker, never the Inbox; the answer is a note on the issue, and only a question that is the person's becomes an ask | NIT-245 |
| **Decide what reaches the person** | "Don't route this through Andrew"; stale carried findings checked by hand | Triage rules (below), on top of what already landed: stale findings re-checked (NIT-214), stalled rounds become signed acceptance requests (NIT-220), accepted findings let land through (NIT-191) | NIT-245 |
| **Carry decisions** | 22 messages beginning "Andrew approved", "Andrew decided", "Product decision" | Gone: the person answers in the Inbox and the asking agent gets the answer (`get_answer`); a goal plan's approval files its issues | done |
| **Recover** | After the freeze, one message to each of 7 lanes: check worktrees for a half-finished rebase or lost commit, rerun any gate or submit whose result you didn't see | The supervisor, at start and on every pass: worktrees checked, interrupted submit jobs run again, orphaned lane locks and runs cleared, hung workers stopped | NIT-246 |
| **Protect the machine** | Paused the heaviest lane when C: lost 16 GB in 40 minutes; Defender at 420% CPU; no real-window tests after the freeze | Code, not messages: the harness refuses to open a window off CI (NIT-188); light worktrees and lanes cap disk (goal 3); the supervisor stops starting workers when the disk or memory is short | NIT-246 |
| **Set duplicates right** | NIT-174 and NIT-175 done by two lanes; NIT-182 sent to the wrong lane because every session had one actor id | One claim per issue, and an actor per session (NIT-194); nothing left to coordinate | done |
| **Product-owner calls** | Rescoped NIT-190 into NIT-224, canceled NIT-192, chose what came after each lane, commissioned the retro and three design docs, proposed goals 4 and 5 | Split: the planner proposes, triage files follow-ups; the person decides goals, scope and order | NIT-244, human |
| **Share resources** | A preview port per lane (5182 to 5185), a notes folder | The supervisor hands each worker a free preview port in its environment | NIT-243 |

## Decisions

1. **No lanes.** A lane was the coordinator's way to give a long-lived chat a run of related
   issues. Crew starts one worker per issue, and continuity comes from the issue body, the handoff
   notes and the project brief. A second attempt keeps the first one's vendor and worktree, as it
   does today. Lanes were never what made the work fast. Parallel workers that do not collide did.
2. **Order comes from the approved goal.** The supervisor works active goals in the order the person
   ranked them (a rank on the goal, set on the goal view; oldest first until they set one), and
   within a goal, the plan's order, which is the order its issues were filed. Blockers are respected
   and the collision check applies. Each goal has a worker budget. A goal with nothing ready yields
   its slots to the next. Work outside any goal runs only when no goal has ready work, except
   priority 1 (exploitable, data loss, a broken release), which goes first wherever it is, as
   `next_task` already ranks it.
3. **The plan names files.** Every planned issue says which files it touches, in its body, where
   `next_task`'s overlap check already reads paths. That is how two workers stay apart before
   either has a branch. It replaces "lanes by area".
4. **Triage handles the routine, and records it; the Inbox stays the person's.** An ask is answered
   by the person only, and that guard stays (`answer_ask` refuses any other actor). A question about
   the board or the code is not an ask: who holds a file, whether an issue is already done, whether a
   carried finding still applies, resubmit or wait. The agent sends it as a triage question, which a
   triage worker answers from the database and the repository with a note on the issue, which the
   asking agent reads and the person can see. Only when the question turns out to be on the closed
   list below does triage file it as an ask, with what it found. Stuck work is triage's too: a land
   conflict goes back to its worker (as Crew does now, NIT-222), and a review that keeps carrying a
   finding is reviewed fresh before it escalates.
5. **What reaches the person is a closed list:** a goal plan to approve; a change that needs their
   approval; a product question (scope, wording, what to build); something irreversible or outside
   the machine (credentials, accounts, licenses, signing, money); a finding an agent wants accepted
   as a trade-off; and a task only they can do (sign a CLI in, grant access, a live check on the
   real desktop). Everything else is triage's, and the morning report counts what triage answered.
6. **Recovery is the supervisor's job, on its own schedule.** It starts at login (NIT-117 already
   keeps it running) and treats every start as a possible crash. Worktrees mid-rebase or with
   uncommitted work go to a worker with a handoff. A submit job whose process died runs again when
   its branch head is unchanged. Lane locks and runs left by dead processes are cleared. A worker
   silent past a threshold is stopped and its issue handed on, up to the retry cap.
7. **A safety rule is code.** The coordinator repeated "no real-window tests on this PC" in 17 messages; the
   harness now refuses instead. When something risky happens again (a disk filling, a harness
   touching the real keychain), the fix is a guard in the product, filed as an issue, not a rule
   in a brief.
8. **The person talks to Nitely, not to a chat.** A goal sentence in the palette, decisions in the
   Inbox (on the phone too), priority and order on the goal view. A question like "why can't I click
   done" or "is the disk the worktrees?" was a product bug or a missing view each time (NIT-220,
   NIT-227, the disk guard), and should be fixed as one.

## What stays the person's

Choosing goals and their order. Approving a plan, and the changes the playbook says need them.
Product judgment: what to build next, rescoping, what "done" means. Anything outside the machine
or hard to undo: credentials, accounts, money, legal text, signing identities, publishing. Tasks
only they can do. And the rules themselves: the playbook changes through
`propose_playbook_change`, applied by the person.

## Build order

1. [NIT-243](nitely://issue/NIT-243): the supervisor works an approved goal in plan order, within
   a per-goal budget, with a preview port per worker. This is the core: with it, a planned goal runs
   with no chat.
2. [NIT-246](nitely://issue/NIT-246): self-recovery and the machine guard (disk and memory short:
   start no new worker, say why). The overnight run depends on it.
3. [NIT-244](nitely://issue/NIT-244): the planner, with files per issue.
   [NIT-245](nitely://issue/NIT-245): triage, with the closed list above. Both can be built in
   parallel with 1 and 2; they touch other files.
4. [NIT-247](nitely://issue/NIT-247): the goal view and morning report, including what triage
   answered and every place the person was needed.
5. [NIT-248](nitely://issue/NIT-248): one real NIT goal, overnight, with no coordinator session.

## How it was measured

Read-only, on 2026-10-18. The board's numbers come from Nitely's database: `event` (issue_created,
landed, land_failed and the person's moves and answers, actor `human`), `review` and `ask`, for NIT
issues in the window. The coordinator's actions come from its session transcript: tool calls of
`SendMessage`, `start_session`, `update_issue`, `file_issue` and `propose_goal`, and the
side-session notices that woke it. The workers' messages come from theirs. Each message was sorted
into one kind by its opening words; the ask classes come from reading all 18. An earlier
coordinator session on the morning of 10-16 (6 sessions started, 10 messages) is not counted.
