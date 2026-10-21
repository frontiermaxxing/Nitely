# The pure cut: every slow test, the decision it checks, and where it goes (NIT-523)

Goal 20 wants the core suite under 60 s in a warm lane, no test over 2 s unless it is a named
scenario, and no protected behaviour lost. This is the map the refactor cards follow. Each card
should be workable from its section here alone.

## Where the time goes

[NIT-522](http://127.0.0.1:7423/i/NIT-522) measured the same tests on two machines
(docs/testing.md, "The time budget"):

- **Windows build lane** (lane run 1666, `e67b215`): 245 core tests over 2 s, 5,320 s summed,
  449 s wall.
- **Linux CI** (workflow run 39638986501, main `11f1516`): 13 core tests over 2 s, 123 s summed,
  31 s wall.

The tests are the same. Three things make them slow:

1. **Process starts, mostly git.** On this PC a git start costs 0.6 to 0.9 s
   ([NIT-518](http://127.0.0.1:7423/i/NIT-518)). A submit makes 30 to 60 git calls, a land 30 to
   45, a queue run about 10 plus 25 per member. This is nearly all the Windows-only time. The cure
   is the pure cut: the decision runs over plain data, and git is called once per fact by a thin
   shell that is tested by one or two scenarios.
2. **The clock.** Sleeps, polls and deadlines: a 5 s announcement cadence, a 10 s TLS deadline, 2 s
   memory-wait sleeps, a 6 s busy-timeout check, 1.1 s sleeps because review times are whole
   seconds, a 20 s `--seconds 20` recording. These are the Linux-slow tests and the load-flaky
   ones. The cure: the decision takes the time (or the reading, or the deadline) as an input, and
   the loop that sleeps is a few lines no test needs to wait through.
3. **A performance store that syncs every statement.** `performance::Store::open`
   (`performance.rs:111-126`) runs about 15 schema statements, each committed and synced
   (`journal_mode=DELETE`, `synchronous=FULL`), and `performance_events::lane` opens it for every
   lane event. Every lane, crew and performance test pays for it on Windows.

## Rules for every card

- **Pure means plain data in, a decision out.** No `Db`, no `Path` that is read, no `Command`, no
  `now()`, no `sleep`. Inputs are small structs a test can write in one line; the output is an
  enum or a struct the shell acts on. Lift existing functions where they already decide (each
  section names them, with line numbers as of main `4af40e4`); do not invent a framework.
- **The shell gathers facts and applies the decision.** It does every git read once (use
  NIT-518's read facade where it has landed: `git_read`), calls the pure function, then does the
  writes. It has no branches of its own beyond "apply what the decision says".
- **Time is an input.** `now: i64` or `since`/`deadline` parameters; a loop that sleeps calls a
  pure step, and its sleep can be injected (`&mut dyn FnMut(Duration)`).
- **Scenarios.** Keep the one or two named per flow below. Put each one's exact test name in
  `.nitely/test-budget.toml` under `[scenarios]`, with what it proves, in the same change that
  makes the rest of its module pure. A scenario may still use git and processes; it should use
  the shared templates ([NIT-527](http://127.0.0.1:7423/i/NIT-527)) and no fixed sleeps.
- **No test is deleted by a refactor card.** Write the pure tests, keep the old ones passing, and
  list in the card's handoff which old test each new one covers (name, what covers it, Windows
  seconds). [NIT-528](http://127.0.0.1:7423/i/NIT-528) gathers those lists and asks Andrew once.
  Where this map says MERGE, the assertions move into the named test in the same card, and the
  emptied test goes on that list too.
- **Behaviour is unchanged.** A card that finds a bug files it; it does not fix it inside the cut.
- **Run only what you change, by name, through `run`.** Full suites go to CI.

Verdicts below: **PURE** (becomes a pure-function test), **SCENARIO** (kept as an end-to-end test
of its flow), **MERGE** (its assertions move into the named test), **CLOCK** (waits on time or
load; the replacement takes the time as an input). "Win" is seconds in lane run 1666; "gone"
means the test no longer exists on main.

## Cards

| Card | Modules | Windows s over 2 s (run 1666) |
| --- | --- | ---: |
| [NIT-524](http://127.0.0.1:7423/i/NIT-524) land and land queue | land_queue, land, land_job, leftovers, slim, light | about 2,000 |
| [NIT-525](http://127.0.0.1:7423/i/NIT-525) submit, review routing, goal branches | submit, submit_job, review, review_batch, periodic_review, project_modes, verification, verification_backstop, verification_audit, verification_timing, rules, proposals, goal_branch | about 1,670 |
| [NIT-526](http://127.0.0.1:7423/i/NIT-526) affected-test selection | affected_tests | 138 |
| [NIT-527](http://127.0.0.1:7423/i/NIT-527) shared repo templates | test_repo, every scenario kept above | (setup of what remains) |
| [NIT-533](http://127.0.0.1:7423/i/NIT-533) lanes and machine admission | lanes, machine, executor | about 450 |
| [NIT-534](http://127.0.0.1:7423/i/NIT-534) crew tick off git | crew, crew_settings, triage | about 280 |
| [NIT-535](http://127.0.0.1:7423/i/NIT-535) performance store and recorder | performance, performance_query, performance_attribution, performance_events, db | about 130 |
| [NIT-536](http://127.0.0.1:7423/i/NIT-536) the CLI's clocks | mcp_concurrent, mcp_stale, older_job, performance_recording, executor CLI, phone | 16 (phone); Linux 2 to 20 s each |
| [NIT-537](http://127.0.0.1:7423/i/NIT-537) the long tail | release_verification, submodules, snapshot, playbook, git, private_terms, install, overlap, release_notes, test_repo | about 600 |

The five new cards (NIT-533 to NIT-537) are filed in goal 20 with this file as their spec. The lanes card touches
`lanes.rs` and `executor.rs`, which [NIT-514](http://127.0.0.1:7423/i/NIT-514) and
[NIT-519](http://127.0.0.1:7423/i/NIT-519) are changing now: start it after they land.

Order: NIT-526 and NIT-524 first (largest saving, and NIT-524's queue decisions are what NIT-525's
batch closing uses), then NIT-525, the lanes card, the performance store (cheap, and every lane
and crew test gets faster), crew, the CLI's clocks, the long tail, and NIT-527 for what remains.

## The five load-flaky tests folded into NIT-523

| Test | What it waits on | Replacement | Card |
| --- | --- | --- | --- |
| executor CLI `machine_transport_runs_snapshot_and_records_executor_without_local_lane` (was [NIT-465](http://127.0.0.1:7423/i/NIT-465)) | a hard 30 s limit polled every 50 ms over `nitely run` → node relay → `nitely executor receive`, which makes about 6 git calls through `process()` (100 ms polls each, up to 2 s in `guard.end`) and a full lane run | a core test of `execute` with the transport injected (`Fn(&WireRequest) -> Result<Reply>` in place of `process(cmd, ...)` at `executor.rs:395`): the row says `executor = machine:test`, no lane, exit 7 kept as 7. The CLI test stays as a named scenario without the 30 s limit, and `process()` waits on the child instead of polling | NIT-533 |
| `performance::tests::a_busy_database_does_not_lose_the_new_collectors_version_announcement` (7 s deadline; was [NIT-466](http://127.0.0.1:7423/i/NIT-466)) | `background()` retries the announcement only after `CADENCE_MS` = 5 s (`performance.rs:14,305`), plus two 250 ms busy timeouts and the store opens, inside 7 s | a pure `recorder_step(announced, open_result) -> Announce \| Acquire \| Wait` lifted from `performance.rs:291-306`: "a failed announcement is retried before ownership is tried", with no clock | NIT-535 |
| `submit::tests::small_changes_handed_in_together_are_reviewed_in_one_batch` (was [NIT-468](http://127.0.0.1:7423/i/NIT-468), [NIT-479](http://127.0.0.1:7423/i/NIT-479)) | **gone**: removed with the review queue in 6e8bd18 ([NIT-509](http://127.0.0.1:7423/i/NIT-509)). It asserted two whole background submits finished within 50 s ("the batch closed when full, not at its window"), so slow gates failed it, not the decision. Batch closing now lives in the land queue (`land_queue.rs:365-380`): wait until `oldest.queued_at + window` (sleeping 500 ms), then take up to `batch_max`, stopping at a different mode authority or a change that lands alone | `plan_run(queue: &[Queued { id, queued_at, authority, alone }], authority, window_secs, max, now) -> WaitUntil(t) \| Alone(id) \| Batch(ids)`, tested for: one change inside the window waits; full before the window closes; window passed with one change; a different authority or a lands-alone change stops the batch; window 0 closes at once | NIT-524 |
| `lanes::tests::a_memory_reason_goes_once_a_request_waits_for_the_checkout` (was NIT-479) | a 2 s sleep after the memory refusal (`lanes.rs:2612`), a 2 s sleep in `wait_for_checkout` (`2774`) with a 60 s give-up on `Instant` (`2770`), a helper thread polling every 50 ms with a 120 s guard, and a full large-repo lane run | the lanes `look` step (below) fed `[Memory(why), CheckoutBusy(why), Admitted]`: `waiting_memory` is cleared and `MemoryWaitEnded` emitted at the second look; the give-up is `checkout_wait(since, now, timeout)` | NIT-533 |
| crew `approved_goals_are_worked_in_the_persons_order_within_each_ones_budget` (was [NIT-349](http://127.0.0.1:7423/i/NIT-349)) | `preview_port` binds real ports from 5190 (`crew.rs:303,655`); every crew test probes the same range in parallel and any process can hold one, so a launch skips a port and "five distinct ports" fails | `preview_port(taken: &[u16], free: impl Fn(u16) -> bool)` with a fake free set, and goal order asserted through `goal_order` alone | NIT-534 |

[NIT-498](http://127.0.0.1:7423/i/NIT-498)'s three approved deletions (ask 68) are applied on
main: the bisection of three (79c4768), the no-origin land at the end of the failed-push submit
test, and the last two synchronous submits of the solution-summary job test.

## NIT-524: land and land queue

### land_queue (`land_queue.rs`)

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `a_bad_change_is_bisected_out_of_five` | 111 | bisection finds the one bad change within 1 + 2⌈log2 n⌉ gates; the others land | SCENARIO (bisection), cut to n = 3 once `Bisect` is tested |
| `what_waits_lands_together_on_one_gate_and_what_cannot_goes_back` | 97 | stackable changes land on one gate; a conflict goes back with its branch and worktree untouched; a change since review goes back | SCENARIO (batch); use `reviewed` instead of `submitted` |
| `explicit_basic_submissions_can_batch_only_with_a_full_combined_gate` | 95 | Basic submissions batch only under explicit `land_batch`, and then the combined gate is Full | PURE |
| `approved_changes_with_an_unchanged_managed_submodule_share_one_gate` | 85 | an unchanged managed submodule pointer may batch; a pointer change either way may not | PURE (`submodules_unchanged`) |
| `a_land_alone_lands_what_queued_behind_it_and_a_local_land_never_goes_in_a_stack` | 80 | a land with push off skips the queue then drains it; a land kept local leaves the queue alone | PURE (`route`) |
| `a_bad_change_is_bisected_out_of_three` | 76 | | gone (79c4768) |
| `a_queue_run_records_each_lands_verification_and_runs_the_after_steps_once` | 76 | each member gets a verification row; after-steps run once per stack | MERGE into `what_waits_lands_together...` |
| `attribution_queued_installs_use_actual_landed_prefix` | 73 | the after-land install is attributed to the landed prefix's project | PURE (`install_scope`) |
| `a_failure_that_names_one_changes_file_costs_one_more_gate` | 63 | gate output naming a file only one change touches sends it back after one more gate | PURE (`suspect`) |
| `changes_wait_again_when_the_branch_moves_during_their_gate` | 60 | main moved during the gate: nothing lands, all requeue | PURE (`after_gate`) |
| `yolo_batch_lands_without_a_gate_pass_or_cached_success` | 55 | test depth Off: the batch lands with the gate skipped | PURE |
| `a_branch_that_gains_commits_during_the_gate_keeps_them_and_goes_back` | 48 | a member whose branch moved goes back with its commits; the rest requeue | PURE (`after_gate`) |
| `approval_revoked_after_the_combined_gate_prevents_any_fast_forward` | 31 | approval revoked after the gate: no fast-forward; that member goes back | PURE (`after_gate`) |
| `a_land_waits_its_turn_and_lands_once_the_lock_is_free` | 31 | a land behind a held lock waits, says "still waiting", and a later land reuses the entry | CLOCK (600 ms `land_waiting`, 500 ms polls) → PURE `entry_for_land` |
| `queued_mode_change_routes_individually_and_refuses_stale_submission` | 26 | a queued row whose mode switched lands alone and is refused as stale | PURE |
| `approvals_large_repos_and_repos_without_lanes_land_alone` | 25 | `lands_alone` reasons | PURE (`route`) |
| `a_left_index_lock_stops_the_run_and_ends_it` | 7 | an old index.lock stops the run before the gate | PURE (`index_lock_verdict`) |

Also heavy and new since run 1666: `a_land_batch_is_reviewed_once_before_its_gate_and_nothing_lands_unreviewed`
(`land_queue.rs:1503`, 4 submits and 2 runs). It may stay as the second batch scenario (the
batched review); otherwise PURE.

**Pure core.** Functions that already decide: `review_requires_individual` (:171), `lands_alone`
(:178), `unchanged_submodules` (:219, mixes git reads), entry choice in `land_waiting`
(:244-249), `reviewed_since` (:302), batch selection in `run_queue` (:357-381), bisection in
`gate_and_land` (:492-565), `gate_failed` (:594, pure), `suspect` (:605), the checks in
`land_prefix` (:704-769), `install_scope` (:894). Proposed:

```rust
struct MemberFacts { key, review: ReviewFacts, approval: ApprovalFacts, mode_authority,
                     land_batch: Option<bool>, checkpoint: bool, submodules_unchanged: bool }
struct BatchPolicy { batch, lanes: bool, large_repo: bool, has_submodules: bool, window_secs, batch_max }
fn route(p: &BatchPolicy, m: &MemberFacts) -> Route            // Queue | Alone(why)
fn plan_run(queue, authority, window_secs, max, now) -> RunPlan // WaitUntil | Alone | Batch
fn entry_for_land(latest: Option<EntryRow>, newest_review_ts) -> EntryChoice // Reuse | Deliver | Enqueue
fn submodules_unchanged(fork: &[Pointer], onto: &[Pointer], head: &[Pointer], policy_changed: bool) -> bool
struct Bisect { .. }  fn next(&mut self, outcome: GateOutcome) -> Step // Gate{prefix} | GateWithout(i) | Finish{bad, land, requeue}
fn after_gate(base_before, base_now, heads: &[(at_stack, now)], auth: &[Result<(), String>]) -> Outcome
fn install_context(projects, issues) -> Context
```

Test `Bisect` exhaustively for n = 1..8 against a fake oracle; that is what lets the real
bisection scenario shrink to three changes.

**Scenarios:** `what_waits_lands_together_on_one_gate_and_what_cannot_goes_back` (the only test
of real stacking, a rebase conflict that leaves the agent's branch alone, one gate, fast-forward
and cleanup) and `a_bad_change_is_bisected_out_of_five` at n = 3.

**Shell reads:** main's branch, index.lock and its mtime, `rev-parse <base>` before and after the
gate; `worktree list` and branches for `branch_of`; per member a rebase-in-progress check,
`status`, the head (one `for-each-ref` for all), patch id and `diff --name-only` of
`onto..tip`; submodule pointers by `ls-tree`; the gate; `log from..tip` for the report.

### land (`land.rs`)

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `later_lands_tidy_what_earlier_ones_left_and_spare_what_is_kept_used_or_linked` | 80 | `--keep` stays; a worktree in use stays whole; a link goes but not its target; the next land tidies leftovers | PURE (`cleanup_plan`, `leftover_verdict`) plus one filesystem test that unlinking never follows a junction |
| `a_land_after_another_waits_for_it_then_lands_or_stops_cleanly` | 66 | `land --after` waits for its blocker, then lands; a conflict stops it; a canceled blocker ends the wait | CLOCK (`NITELY_LAND_JOB_POLL_MS`, a process-wide variable that leaks into other tests; 400 ms sleep; waiter thread) → PURE `blockers_ready` |
| `appended_test_files_keep_only_the_original_signed_approval` | 59 | a test-list-only drift keeps the signed approval; any other change is refused | PURE (`decide_approval`) |
| `lands_only_with_the_apps_signed_approval_of_the_patch_it_saw` | 56 | only an app-signed approval of the reviewed patch lands | CLOCK (two 1.1 s sleeps: approval times are whole seconds) → PURE (`standing_approval`, signature check) |
| `when_the_repo_cannot_move_the_submodule_does_not_either` | 54 | if the repo's fast-forward is refused, the submodule's main and checkout are put back | MERGE into `a_change_that_spans_a_submodule...` |
| `a_finding_the_person_accepted_does_not_block_the_land` | 52 | a P1 blocks unless accepted with a signed acceptance | PURE |
| `lands_by_nitely_when_the_playbook_says_so` | 50 | `by = auto` lands in submit; a human-needed change lands on approval in its own repo | PURE (routing); the approve-then-land step MERGEs into `lands_reviewed_work...` |
| `lands_reviewed_work_and_waits_for_human_approval` | 47 | a reviewed change lands, pushes, runs teardown, cleanup and matching after-steps; a billing change waits | SCENARIO (single land) |
| `a_change_that_spans_a_submodule_lands_both_branches_together` | 46 | repo and submodule branches land together | SCENARIO (submodule land) |
| `a_change_that_leaves_the_submodule_alone_lands_as_any_other` | 40 | a change outside the submodule never moves its main | PURE (the moving set in `submodules::rebase`) |
| `lands_only_what_a_human_approved_when_the_playbook_says_human_and_leaves_it_to_the_agent_by_author` | 36 | `by = human` waits for approval; `by = author` leaves the land to the agent | PURE |
| `a_dead_lands_lock_is_taken_over_and_a_left_index_lock_is_named` | 34 | a dead holder's lock is taken over; an old index.lock stops the land; a fresh one means git runs | PURE (`lock_state`, `index_lock_verdict`) |
| `submit_says_to_move_a_submodule_checkout_back_after_a_rollback` | 31 | submit advises `reset --keep` after a rolled-back pointer | PURE (`submodule_advice`) |
| `submit_says_to_update_a_submodule_checkout_a_rebase_left_behind` | 30 | submit advises `merge --ff-only` | MERGE with the test above into one table |
| `a_finding_sent_back_stays_a_defect_to_fix` | 18 | ask and decline semantics | PURE (database only; drop the worktree) |
| `an_automatic_land_that_stops_on_a_conflict_sends_the_issue_back` | 17 | an automatic land stopped on a conflict sends the issue to In Progress | MERGE into `an_automatic_land_whose_gate_ran_past_the_lane_limit...` (already pure, `land.rs:1641`) |
| `refuses_changes_after_review_conflicts_and_a_held_lock` | 17 | refuses a patch changed since review, leaves a conflicting rebase, refuses a live lock | PURE |
| `steps_outside_the_lock_build_the_newest_commit_and_fold_lands_that_finish_meanwhile` | 16 | outside-lock after-steps run at the newest commit and fold | PURE (`after_steps`) plus one filesystem check of the pending mark |
| `a_finding_the_rounds_keep_raising_keeps_one_open_request` | 16 | ask dedupe | PURE (database only) |
| `submit_refuses_submodule_work_the_branch_does_not_point_at` | 15 | submit refuses submodule commits the branch does not point at | PURE (`check_pointers` as a verdict) |
| `a_killed_lands_index_lock_is_cleared_and_anyone_elses_named` | 15 | index.lock cleared only when the mark names a dead holder | PURE (`index_lock_verdict`) |
| `a_stalled_review_asks_about_each_finding_and_waits_for_all_of_them` | 7 | | PURE (database only) |
| `warns_when_a_dependency_landed_that_no_after_step_set_up` | 4 | a lockfile landed with no after-step | PURE (`dependency_warnings`) |
| `two_blockers_in_one_function_are_two_requests` | 4 | | PURE (database only) |
| `two_requests_for_one_finding_become_one_and_the_newest_stands` | 3 | | PURE (database only) |
| `removes_its_own_worktree_however_its_path_is_spelled` | 3 | `cleanup` knows its worktree under 8.3 or `/private` spellings | PURE (`made_by_layout` over normalized strings) |

**Pure core.** Already deciding: routing in `land` (:98, :151-153), the refusal checks in `run`
(:862-870, :949-958), `check_approval_at` (:1147), `check_yolo_policy` (:1221),
`check_disabled_policy` (:1235), `Db::standing_approval` (:1251), `after_auto_land` (:434),
`nitely_lands_approved` (:465), `approval_repo` (:539), `left_index_lock` (:388),
`refused_fast_forward` (:408), `clear_killed_fast_forward` (:656), `Lock::take` and
`held_by_dead` (:1424, :1463), `after_land` (:599), `after_land_outside_lock` (:710), the keep
rule in `cleanup` (:1289-1293), `made_by_layout` (:1331), `clean_up_landed` (:1067), `branch_of`
(:1102); `leftovers::kept_on_purpose` / `not_ours` / `why_it_stays` (:127, :151, :190),
`land_job::wait_for_blockers` (:229), `submodules::check_pointers` / `rebase` / `unchanged`
(:264, :331, :414). Proposed:

```rust
fn decide_approval(i: &ApprovalInput) -> Result<ApprovalNote, Refusal>
fn standing_approval(approved_at: Option<i64>, reviews_newest_first: &[ReviewFacts]) -> Option<i64>
fn land_route(push, has_origin, alone: Option<Reason>, goal_branch, queue_waiting) -> (Path, DrainQueue)
fn lock_state(text: Option<&str>, pid_alive: impl Fn(u32) -> bool, age_secs, break_lock) -> LockDecision
fn index_lock_verdict(lock_mtime, land_began, grace, mark: Option<(bool, SystemTime)>) -> IndexLock
fn after_steps(changed: &[String], steps: &[GateStep], under_lock: bool) -> Vec<(Step, Run | Skip | Defer)>
fn cleanup_plan(temporary, made_by_layout, keep, crew_in_it, holders) -> Cleanup // Keep | KeepWhole | Remove
fn leftover_verdict(facts: LeftoverFacts) -> Option<String>
fn blockers_ready(states: &[(key, state, landed)]) -> Ready                       // Wait | Go | Over(why)
fn submodule_advice(pointer, checkout_head, on_integration, pointer_is_ancestor) -> Option<Advice>
```

Approval and review ordering should compare row ids, not whole-second times; that is what
removes the 1.1 s sleeps here and in submit. The six database-only tests need an issue and review
rows, not `f.reviewed` and a worktree.

**Scenarios:** `lands_reviewed_work_and_waits_for_human_approval` (real push, teardown,
after-steps, worktree removal; fold in approve-then-land) and
`a_change_that_spans_a_submodule_lands_both_branches_together` (fold in the refused attempt from
`when_the_repo_cannot_move...`).

**Shell reads:** main's branch and `rev-parse <base>` (before, after the gate, before the
fast-forward), the git dir for index.lock and the fast-forward mark; the change's worktree,
rebase-in-progress, `status`, `merge-base --is-ancestor`, the rebase (an effect), heads before
and after the gate; patch id, the test-command equivalence diff, `diff --name-only`; submodule
pointers; `log base..head`; process holders of the worktree.

### slim and light

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `slim::reclaims_only_finished_worktrees_and_says_why_it_keeps_the_rest` | 69 | reclaim only clean, landed or pushed worktrees not at work or in use; only ignored, untracked build dirs | PURE (`why_kept(&WorktreeFacts)`, `is_build_dir`), plus the test cut to two worktrees as the SCENARIO |
| `light::a_sparse_worktree_holds_the_playbook_s_and_the_issue_s_directories_and_widens` | 13 | the sparse cone and what `widen` adds | PURE (`sparse_dirs`, `widen_plan`) |
| `light::a_light_worktree_runs_no_setup_and_gets_a_reference_tree_its_gate_sets_up_once` | 10 | no setup in a light worktree; the reference tree; the gate sets up once | SCENARIO (start it from a sparse playbook so one real sparse round is covered) |
| `light::heavy_commands_in_a_light_worktree_with_lanes_get_a_reminder` | 10 | which shell lines get the "use run" reminder | PURE (`nudge_for(is_light, has_lanes, heavy, line)`) |
| `light::a_new_branch_starts_from_the_scheduled_base_not_the_tip` | 5 | a new branch starts from the scheduled base | PURE (`lanes::scheduled_base` over tip, record, schedule, now) |
| `light::full_stays_the_default_and_lanes_sit_beside_the_worktrees` | 4 | full mode is the default; `lanes_dir` | PURE |

Slim: `at_work` (:108), `keep_dirs` / `plain` (:51, :69) and `human` (:205) are pure already;
take one process-list snapshot for all worktrees instead of one per worktree. Light: `dedupe`,
`commands_of`, `is_lane_run`, `folder_of`, `heavy_in`, `lanes_dir`, `is_lane_path`, `wanted` are
pure already.

## NIT-525: submit, review routing, goal branches

### submit (`submit.rs`)

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `basic_submission_and_land_run_only_basic_checks_and_enqueue_an_audit` | 104 | Risk depth gives Basic for an inert change; land enqueues an audit | SCENARIO (reduced checks, land, audit) |
| `disabled_review_keeps_gate_and_landing_without_synthetic_review_or_fallback` | 93 | review off: verdict `disabled`, In Review, changed branch refused, lands | MERGE into the scenario above, plus PURE; its loop over four batch modes is moot since the review queue went |
| `submits_with_a_review_and_routes_flags_to_a_human` | 77 | a passing review with a flag routes to a human; pushed and local; no-changes refused | SCENARIO (submit, review, push) |
| `a_patch_that_passed_is_not_reviewed_again_and_a_rewritten_one_starts_from_the_pass` | 75 | a pass is reused only for the same diff id, rules and reviewer | PURE (`reusable`, from `submit.rs:790-795`) |
| `a_batched_projects_submit_leaves_the_review_to_the_land_batch_unless_it_must_be_reviewed_alone` (was `a_change_that_cannot_join_a_batch_is_reviewed_alone`) | 74 | a batched mode records `batched`; a playbook change is reviewed alone | PURE (`why_alone`, `review_batch.rs:33`) |
| `tests_and_review_off_skip_automatic_checks_but_preserve_manual_checks_and_restored_policy` (was `yolo_skips_...`) | 66 | tests and review off: gate skipped; `check_approval` refuses a retired verdict, a mode change and a blocked path | PURE (a `check_approval` matrix); its land MERGEs into the basic scenario |
| `a_failed_push_hands_nothing_in_and_a_repo_without_origin_is_local` | 63 | a failed push stays In Progress; no origin is In Review locally | PURE (`next_state`, from `submit.rs:455-481`) |
| `the_same_edit_somewhere_else_is_reviewed_though_its_patch_id_is_the_same` | 61 | the diff id tells two same-patch-id edits apart | PURE (`reusable`) plus a small git test of `diff_id` alone |
| `the_fresh_review_is_told_what_the_person_accepted_so_a_reworded_repeat_does_not_reach_them` | 60 | a stalled run gets a fresh review told what was accepted | PURE (`settle_fresh`, from `submit.rs:335-368`; `accepted_told`, `review.rs:269`) |
| `a_p1_that_two_rounds_leave_open_goes_to_a_person` | 43 | `not_converging` sends it to Needs You; declining resets the count | CLOCK (two 1.1 s sleeps) → `rounds_since` by row id; then SCENARIO (rounds stall) |
| `a_cross_vendor_rule_sends_a_same_vendor_review_to_a_human` | 41 | `independence_reason` routes to a human | PURE (`review.rs:363`, `:423`) |
| `a_second_round_checks_the_first_rounds_finding` | 36 | round 2 rechecks and carries an open finding | MERGE into `review::tests::a_re_review_carries_what_is_not_fixed` |
| `custom_mode_controls_tests_and_review_independently_and_invalidates_old_evidence` | 35 | test depth and pre-review decide separately; a mode change invalidates evidence | PURE (`Options::covers`, `project_modes.rs:139`) |
| `a_finding_only_the_rounds_carry_is_checked_fresh_before_it_goes_to_a_person` | 28 | a fresh review that finds nothing passes with no ask | PURE (`settle_fresh`) |
| `disabled_review_leaves_reviewer_conditions_unevaluated_and_other_approvals_intact` | 22 | review off: reasons say unevaluated; Needs You | PURE (`unevaluated_reasons`, `review.rs:97`) |
| `a_signed_out_reviewer_is_passed_over` | 21 | a reviewer that could not run is skipped | PURE (`choose_after`, `review.rs:369`; `says_signed_out`, `:322`) |
| `a_shadow_batch_starts_from_the_round_before_the_review_that_decided` | 7 | `previous_round_at(before)` | MERGE or retire: only this test uses `before: Some(_)` since shadow batches went ([NIT-499](http://127.0.0.1:7423/i/NIT-499)); put it on NIT-528's list |
| `small_changes_handed_in_together_are_reviewed_in_one_batch` | 33 | | gone (6e8bd18); see the load-flaky table |
| `a_shadow_batch_is_recorded_beside_the_single_reviews_and_routes_nothing` | 78 | | gone (0422621) |
| `disabled_policy_during_a_failed_review_prevents_fallback_launch` | 20 | | gone (657dca6) |

### submit_job, review, review_batch

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `submit_job::solution_summary_is_bound_to_the_revision_handed_in` | 66 | the summary stands only for the handed-in head and patch | PURE (`solution_applies(handed_in, gate_commit, explained_patch, submitted_patch)`) |
| `submit_job::a_job_whose_process_is_gone_is_failed_and_one_that_cannot_start_runs_here` | 65 | a dead holder fails the job; a runner error runs it here | PURE (`abandoned(job, pid_alive, cmdline, now)`, from `submit_job.rs:182-209`); run-here MERGEs into the scenario |
| `submit_job::solution_summary_does_not_follow_a_changed_patch_at_the_same_head` | 53 | same head, other patch: the summary is dropped | PURE (same table) |
| `submit_job::a_submit_hands_in_at_once_and_the_job_brings_the_result_back` | 39 | enqueue, supersede, run, deliver | SCENARIO (async submit) |
| `submit_job::a_failed_submit_comes_back_only_while_its_issue_waits_for_it` | 10 | `returned_submit` | PURE (`submit_job.rs:86`) |
| `review::runs_a_reviewer_command` | 13 | reviewer exits: pass, signed out, exited, unfinished, did not start | CLOCK (a 250 ms `try_wait` poll, `review.rs:860`, is a floor on every launch) → PURE `classify_reply`, keeping one real launch |
| `review::an_open_finding_must_point_at_code_still_on_the_branch` | 7 | an open finding must cite code still there | PURE (`CurrentCode` from a file list and a reader) |
| `review_batch::an_interaction_stays_open_until_the_first_change_landed_under_the_second` | 8 | held until the first is Done and an ancestor | PURE (`interaction_status`, split from `review_batch.rs:388`) |
| `review_batch::a_change_whose_worktree_is_gone_is_read_at_its_own_commit` | 8 | `checkout_at` clones at the change's commit | SCENARIO (small) |
| `review_batch::an_interaction_lets_the_first_land_and_sends_the_second_back`, `...reaches_a_change_reviewed_alone_after_all` | 3, 3 | | gone (6e8bd18) |

**Pure core for submit:** `next_state(stalled, verdict, push_failed, approval_stands, approver,
by_nitely, batched, yolo, review_disabled, gate_skipped) -> (state, next)` from
`submit.rs:455-481`; `outcome(...)` from `:427-440`; `accepted_only` (the same logic is at
`submit.rs:303-309` and `review_batch.rs:261`); `settle_fresh`; `rounds_since(reviews,
decided_at)` by row id, feeding `not_converging` (`:687`, pure); `reusable`; the reasons
(`human_reasons`, `independence_reason`, `flag_reasons` `review.rs:756`, `unevaluated_reasons`);
`why_alone` with its lookups passed in; `solution_applies`; `abandoned`; `returned`.

**Scenarios:** `submits_with_a_review_and_routes_flags_to_a_human`,
`basic_submission_and_land_run_only_basic_checks_and_enqueue_an_audit`,
`a_p1_that_two_rounds_leave_open_goes_to_a_person` (after `rounds_since`),
`submit_job::a_submit_hands_in_at_once_and_the_job_brings_the_result_back`, and one real reviewer
launch.

**Shell reads:** worktree and branch, `rev-parse HEAD`, `status`, `diff_base`, `rev-list
--count`, `diff --name-only`, patch id and diff id (both from one `git diff`), the previous
round's ancestry, `has_origin`, the push, the gate.

### periodic_review, verification, rules, proposals

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `periodic_review::periodic_scenario_threshold_failure_retry_reopen_and_duplicate_finding` | 73 | threshold, failure and retry, reopen, existing, filed | SCENARIO |
| `periodic_review::periodic_durable_completion_preserves_new_landings_and_replays_card_actions_once` | 27 | completion is fenced; restart replays once | SCENARIO (crash and replay); its action choices PURE |
| `periodic_review::periodic_exact_accepted_finding_records_decision_without_reopening_or_filing` | 13 | an accepted finding gets `accepted` | PURE (`finding_action`, from `periodic_review.rs:565-671`) |
| `periodic_review::periodic_last_attempt_crash_becomes_retryable_without_advancing_cursor` | 13 | a crashed job under our lock fails; retry; ownership | PURE (`job_step`, from `:357-375`) |
| `periodic_review::stored_periodic_policy_inherits_defaults_and_binds_live_authority` | 12 | stored policy plus defaults; fingerprint | PURE (`rules::effective`, `rules.rs:196`) |
| `project_modes::independent_depth_controls_execute_real_commands_and_manual_gate_stays_full` | 27 | Basic runs basic commands; the manual gate stays Full; Off skips | PURE (`project_plan`'s Basic branch, `verification.rs:414-428`); real commands MERGE into the basic scenario |
| `project_modes::full_backstop_runs_without_shared_lanes_and_only_custom_stops_it` | 8 | the backstop runs with lanes off | MERGE into `verification_backstop::a_head_known_broken...` plus PURE |
| `verification::a_test_inside_a_mapped_component_runs_its_checks_and_test_infrastructure_stays_full` | 16 | a mapped test runs its component's checks; test infrastructure is Full | PURE (`plan`) |
| `verification::fast_and_careful_run_only_quick_checks_before_landing_except_at_a_risk_floor` | 14 | Fast gives Integration; sensitive or protected is Full | PURE |
| `verification::active_basic_authority_tracks_integration_policy_and_actual_branch_diff` | 14 | base is the merge-base, policy main's tip | PURE |
| `verification::revision_policy_and_declaration_bound_advice_never_grants_a_gate_exemption` | 12 | the base commit's policy is used, never the candidate's | PURE |
| `verification_backstop::inverse_preparation_resumes_stale_base_staged_changes_and_committed_inverse` | 12 | `prepare_inverse` steps | SCENARIO, with `inverse_step` PURE |
| `verification_backstop::isolation_proves_single_culprit_and_rejects_interactions_and_irreversible_paths` | 11 | bisect, reversible paths, interactions | PURE (`isolate_with` :406 takes an oracle; `reversible`) |
| `verification_backstop::a_head_known_broken_is_not_run_again_and_the_next_landing_is_checked_at_once` | 10 | a broken head is not rerun; the next is checked at once | SCENARIO (backstop) |
| `verification_backstop::observations_never_regress_and_unrecorded_tip_blocks_reduced_landing`, `authoritative_backward_reset_invalidates_the_old_green_baseline`, `ordinary_projects_are_quiet_but_removed_recorded_policy_fails_closed`, `reconciliation_recovers_counter_and_original_deadline_after_interrupted_landing` | 4, 3, 3, 3 | observe compare-and-swap; reset; optional policy; counting | PURE (`observe_update`, `optional_policy`, `due`) |
| `verification_audit::executable_worker_reviews_entire_landed_diff_with_independent_command` | 9 | the worker claims a job and runs an independent reviewer | SCENARIO |
| `verification_audit::audit_requires_immutable_clean_checkout_and_fresh_result` | 3 | | PURE (`checkout_ok`) |
| `verification_timing::comparisons_require_the_same_candidate_policy_and_latest_successful_execution` (crew) | 4 | | PURE (`compare(rows, policy_ids)`) |
| `rules::committed_reviewer_save_requires_exact_preview_clean_playbook_and_land_lock` | 50 | `save_reviewer` refusals; rollback on index.lock, a ref lock or a failing hook | PURE (`save_preconditions`) plus a trimmed SCENARIO for the rollbacks; use `test_repo::copy`, not a raw `git init` |
| `proposals::approving_a_committed_playbook_commits_it_on_the_integration_branch` | 8 | | SCENARIO |
| `proposals::a_stale_integration_branch_is_fixed_on_the_branch_the_change_names` | 9 | | PURE (`committed_branch`, `diff`) |
| `proposals::a_local_edit_of_the_committed_playbook_stops_the_commit` | 5 | | PURE (`commit_target`) |

**Verification's pure core:** `plan(facts: DiffFacts, declared, affected: Selection, settings,
disabled_rules) -> Plan`, lifting `assess_with_mode` (`verification.rs:200`), `assess_inner`
(:273), `project_plan` (:401) and `fast_checks` (:363) off git. `prove`, `risk_floor`,
`overlay_verification` and `Options::covers` are pure already. The shell does one `rev-parse`,
`merge-base`, one `diff --raw --numstat -z`, one `cat-file --batch`, and the selection.

**Goal branches** ([NIT-525](http://127.0.0.1:7423/i/NIT-525)'s third part) had no test over 2 s
in run 1666; on Linux
`goal_branch::tests::items_land_into_the_goal_branch_and_a_checkpoint_lands_the_slice_on_main`
took 2.6 s. Keep it as the goal-branch SCENARIO and make the item-target and checkpoint-trigger
choices pure beside it.

## NIT-526: affected-test selection

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `the_repos_own_map_selects_the_frontend_and_keeps_everything_else_full` | 64 | the real map: web, scripts and new tests go to web checks; cli, app and crew to their crates; build inputs, core Rust, the map, manifests, mixed or unmatched tests stay Full | PURE (one cached `ls-tree` of the repo, the map by `include_str!`, real manifests) |
| `dynamic_import_missing_consumer_and_candidate_policy_fail_closed` | 15 | non-literal import, uncovered source, changed map, uninventoried test: Full | PURE |
| `a_build_script_reduces_only_with_its_inputs_approved_and_unchanged` | 15 | `build.rs` reduces only if approved and unchanged | PURE |
| `consumers_outside_component_and_unknown_policy_fields_are_full` | 10 | a consumer outside every component, or an unknown field: Full | PURE |
| `reverse_imports_select_helpers_and_tests_but_execute_whole_suite` | 9 | the reverse graph through a helper; whole suite; the Selection's identity | SCENARIO (the only test of base, head, tree and policy blob against real git); add a policy commit that differs from base (`affected_tests.rs:425`, untested today) |
| `cargo_explicit_mapping_selects_target_and_shared_helper_consumers` | 9 | input map gives the lib filter; a shared helper adds the integration target | PURE |
| `declared_component_consumers_run_all_their_suites` | 6 | `depends_on` pulls in the consumer | PURE |
| `approved_whole_suite_mapping_handles_opaque_inputs_without_claiming_a_graph` | 6 | explicit inputs allow an opaque import | PURE |
| `deleted_source_keeps_previous_consumers` | 4 | a deleted file's old edge still counts | PURE |

**What it reads today.** All through `playbook::git`, never the working tree: one `rev-parse` of
base, head and head's tree (`affected_tests.rs:255`); `ls-tree -r -z` of base, head, and the
policy commit when it differs (`snapshot()` :167, called at :422-425; the changed set is the
difference, :429-436); the map's blob (:427); each selected Cargo suite's `Cargo.toml` (:637,
uncached, so a manifest shared by three suites is read three times); and for an import graph
(:462-502) every `tsconfig*.json` (:786), every source by `blobs::read` (`cat-file --batch`,
:806), and the nearest `package.json` for each bare import (:858, re-read every time).

**The decision** is `select_inner` (:421-612): changed set; per-path checks (protected, not a
regular file, build-script input, no owner, :441-458); JS mode (:462-502); reverse-import closure
(:503-514); owners and approved inputs (:515-529); `depends_on` (:530-540); checks and "no test
consumer" (:542-608). `validate` (:281-412) is pure over `Snapshot`; `cargo_check` (:630-734) is
pure but for :637; `js_graph` (:762-838) but for :786, :806-818 and :831. The `ls-tree` parse in
`snapshot()` (:169-187) lifts to `parse_ls_tree(&str) -> Result<Snapshot>`.

```rust
pub(crate) trait Objects {   // git: today's read() and blobs::read; tests: a HashMap<oid, String>
    fn text(&self, oid: &str) -> Result<String>;
    fn texts(&self, oids: &[String], max_file: usize, max_total: usize) -> Result<Option<HashMap<String, Blob>>>;
}
pub(crate) fn decide(policy_text: &str, before: &Snapshot, after: &Snapshot, objects: &dyn Objects) -> Decision
```

The shell keeps the `rev-parse`, the `ls-tree` calls and the map's blob, then calls `decide` with
a git-backed `Objects`, read lazily. A test helper `mem(&[(path, content)]) -> (Snapshot,
MemObjects)` builds both sides in memory; each test's file sets are the ones its fixture commits
today. Also cache blob text by object id within one selection (the manifest and `package.json`
re-reads).

## NIT-533: lanes and machine admission

Starts after NIT-514 and NIT-519 land (both change `lanes.rs`).

| Test | Win | Linux | Decision | Verdict |
| --- | ---: | ---: | --- | --- |
| `a_setup_that_failed_runs_again_and_a_dead_requests_command_is_stopped` | 33 | 1.1 | setup recorded only on a pass; a leftover stopped only when pid, start and command match | PURE (`setup_holds`, `lanes.rs:3122-3127`; `is_leftover`, :434-451); the kill goes to a containment scenario |
| `a_passing_run_of_the_gates_commands_answers_the_gate` | 23 | 0.1 | an earlier run answers a gate only for the same tree, commands, a pass, a local executor | PURE (`answers_gate`, from `passed_on_tree` :1050) |
| `a_memory_reason_goes_once_a_request_waits_for_the_checkout` | 23 | 4.1 | | CLOCK; see the load-flaky table |
| `a_lane_runs_requests_keeping_only_its_build_dirs` | 19 | 0.2 | the lane round trip | SCENARIO (lane) |
| `a_request_waits_while_the_main_checkout_has_changes_and_says_why` | 19 | 2.1 | a busy checkout gives a reason, then gives up | CLOCK → `checkout_wait` plus `look`; MERGE with the test above |
| `the_gate_runs_in_a_lane_once_per_tree_and_land_goes_first` | 18 | 0.2 | gate reused per tree; a flake recorded; land goes first | MERGE into the lane scenario |
| `a_request_runs_in_the_main_checkout_and_puts_it_back_on_its_branch` | 17 | 0.1 | large repo: lend the checkout, put it back | SCENARIO (large repo) |
| `a_keep_name_matches_at_any_depth_and_a_path_only_from_the_root` | 17 | 0.1 | `keep` matching | PURE (`in_keep` :212, `keep_anywhere` :2996 are pure) |
| `a_file_that_appeared_during_the_request_keeps_the_checkout_from_going_back_over_it`, `an_ignored_file_where_the_commit_needs_a_folder_is_refused`, `a_commit_that_would_overwrite_an_ignored_file_is_refused` | 17, 15, 13 | ≤0.2 | do not go back over the person's files | MERGE → PURE `would_overwrite(commit_paths, persons_files)` (:255) |
| `a_file_the_request_made_a_folder_comes_back_as_a_file` | 16 | 0.1 | | MERGE into the large-repo scenario |
| `a_checkout_a_dead_request_left_detached_goes_back_unless_it_changed`, `after_land_steps_never_run_on_a_requests_commit` | 16, 11 | 0.1 | recover a lent checkout unless changed | PURE (`recover_decision(lent_mark, changed)`, :401) |
| `a_snapshot_is_head_or_a_commit_of_the_worktree_without_touching_it` | 15 | 0.1 | snapshot | SCENARIO (snapshot) |
| `a_snapshot_leaves_the_worktrees_index_alone_even_when_killed` | 13 | 0.2 | | MERGE into the snapshot scenario |
| `a_request_waits_for_its_repo_and_a_slot_but_not_for_a_busy_repo`, `a_request_waiting_for_its_checkout_holds_up_no_other_project`, `an_older_schedulers_request_holds_up_no_newer_one` | 13, 4, 2 | 0 | queue turns | PURE (`turns`, :2142, over rows) |
| `a_request_waits_for_memory_and_says_so`, `a_floor_lowered_while_a_request_waits_lets_it_go`, `a_memory_reason_goes_once_a_request_waits_for_a_slot` | 12, 11, 10 | 4.1, 4.1, 2.4 | a refusal records its reason; limits re-read each look | CLOCK (2 s sleeps, :2612) → `look` and `admit` |
| `a_snapshots_playbook_never_counts_when_the_branch_has_none`, `while_a_request_has_the_checkout_the_rules_come_from_its_branch`, `the_persons_own_untracked_playbook_stays_in_force_while_lent` | 10, 9, 6 | 0.1 | where the playbook comes from while lent | PURE (`playbook_source(lent, branch_has, on_disk)`, :331) |
| `a_lane_command_takes_what_it_started_with_it`, `a_command_past_its_time_is_stopped`, `a_run_over_its_memory_cap_is_stopped_as_out_of_memory_in_the_lane` | 10, 3, 2 | -, 1.0, 1.2 | containment: children, time limit, memory cap | SCENARIO (containment; one test, better in `procs`) |
| `the_main_checkout_is_ready_only_when_clean_and_on_its_branch` | 9 | 0.1 | `checkout_busy` reasons | PURE (over branch, changed paths, keep) |
| `a_lane_command_keeps_its_temporary_files_beside_the_lane` | 8 | 0.1 | per-request TEMP and NITELY_HOME | MERGE into the lane scenario |
| `the_base_moves_only_on_its_schedule` | 8 | 0 | | CLOCK → `base_due(recorded_at, now, every_hours)` (:153) |
| `a_command_gets_the_machines_share_of_cores` | 7 | 0.1 | | PURE (`budget_for`, `share_env`) |
| `hygiene_trims_a_lane_over_its_cap_and_builds_cold_over_twice_it` | 6 | 0 | size cap and baseline | PURE (over sizes and the record, :2881, `cap_bytes` :2855) plus one link-safety check |
| `the_overview_keeps_each_lanes_last_problem_however_many_runs_since` | 5 | 0.3 | the last problem per lane | PURE over rows (or one transaction for its 680 inserts) |
| `a_gate_outside_lanes_takes_a_machine_slot` | 3 | 0 | `hold_machine` rows | PURE |
| `machine::admit_waits_until_memory_frees_and_records_the_start` | 4 | 4.0 | wait while memory is low, then record the start | CLOCK (two 2 s sleeps, `machine.rs:216`) → `admit(..., sleep)`, `admit_once_with(..., now_ms)` |
| `executor::receiver_reconstructs_binary_snapshot_runs_once_and_refuses_cross_project` | 25 | 1.3 | rebuilds the binary patch, runs once per id, refuses other projects | SCENARIO; `process()` waits on the child instead of polling every 100 ms (`executor.rs:181-197`) |

**Pure core:** one `look(prev: Waiting, obs: Look) -> (Waiting, Vec<RowUpdate>, Vec<perf::Kind>)`
with `Look = NotMyTurn | CheckoutBusy(why) | NoSlot | NoLane | Memory(why) | Admitted`, lifted
from `take_turn` (`lanes.rs:2558-2629`) and its copy in `hold_machine_checked` (:2713-2738); the
loops call it with an injectable sleep. `admit(memory, limits, since_last_start, sizing,
quick_slot, reserved) -> Option<String>` combines `machine::admission_for` (`machine.rs:87`,
pure), the sum in `caps_fit` (`lanes.rs:1536-1549`) and `sizing`. `turns`, `LiveRun::next`,
`cap_bytes` and `budget_for` are pure already. Shared cost for every lane test: `run_logged`
polls every 200 ms (`lanes.rs:3330`); waiting on the child would remove that floor.

## NIT-534: the crew tick off git

The crew's clock is injected already and its agents are fakes; what costs is that
`Fake::prepare` runs a real `playbook::start_task` (worktree add and setup) for every worker, and
git in `current_branch`, `branch_vendors`, `worktree_state` and cleanup. Give the tick a `Repo`
trait (`start_task`, `current_branch`, `branch_vendors`, `worktree_state`, `clean_up`) with a
fake that makes the folder and marks setup passed, and lift the decisions:
`why_not_starting(counts, rules, moment)` (`crew.rs:503`), `goal_order` (:622), `end_of(exited,
now, started, last_sign, rules, landing)` (:946-958), `setup_end` (:738-751), `adopt_setup`
(:889-912), `disposition` from `settle_database` (:1007), `after_land(outcome, now, last_try)`
(from `finish_landings`, :1311), `preview_port(taken, free)` (:650), `other_branch` and
`branch_vendors` over names (:1477, :1526), `current_branch` over its head files (:1485).

All 33 crew tests over 2 s (4 to 21 s each on Windows, none on Linux, where crew did not run) are
PURE under that split, except:

- SCENARIO: `keeps_the_cap_and_starts_the_next_when_a_slot_frees` (cap and fill end to end) and
  `a_landed_workers_worktree_and_branch_are_removed_once_it_exits` (real cleanup), with
  `clean_up_touches_only_a_listed_worktree_on_its_branch` merged in.
- `focus_reclamation_waits_for_the_whole_settlement_transaction` stays a database concurrency
  test (no git).
- `approved_goals_are_worked_in_the_persons_order_within_each_ones_budget`: see the load-flaky
  table.

Also here: `crew_settings::saves_a_stored_playbook_in_place_and_commits_a_repos_own` (14 s: PURE
`save_target` from `save_with` :250, plus one commit scenario) and
`triage::disabled_pr_review_still_allows_triage_answers_and_escalation` (8 s: MERGE into the
`triage_answers_the_routine...` scenario; `look` with a fake runner).

## NIT-535: the performance store and recorder

Give tests an in-memory `Store`, or create the schema in one transaction, and the Windows cost of
every lane, crew and performance test drops. Then:

- `performance_query` (10 tests, 4 to 10 s each on Windows): PURE; `query_at` takes `now`
  already, and `episodes`, `sample_gap` and `compact` (`performance_query.rs:221-299`) are pure.
- `performance_attribution` (7 tests, 2 to 8 s): PURE `admit_workload(version, settings, exists,
  create, count)` from `performance_attribution.rs:514-537`; `Sampler::attribute` (:640) is pure.
  Keep `attribution_standalone_gate_registers_issue_and_job_accounting` and
  `attribution_native_fixture_keeps_short_lived_job_accounting` as Windows SCENARIOs.
- `performance_events` (3 tests, 5 to 6 s): PURE `keep_event(version, settings, prior, kind)`
  from `performance_events.rs:39-46`; keep one race test on a real database.
- `performance::a_busy_database_does_not_lose_the_new_collectors_version_announcement`: see the
  load-flaky table.
- `db::a_write_waits_out_a_long_write_of_another_connection` (6 s on both): CLOCK → assert the
  `busy_timeout` pragma equals `BUSY_TIMEOUT` (`db.rs:286,648`) instead of sleeping 6 s.
- `db::processes_opening_an_old_database_together_upgrade_it_once` (10 s): SCENARIO, cut to one
  or two rounds.

## NIT-536: the CLI's clocks

These are the Linux-slow ones, in `crates/nitely-cli/tests` and `phone`:

| Test | Linux | What waits | Replacement |
| --- | ---: | --- | --- |
| `performance_recording::performance_record_duration_stops_cleanly` | 20 | `--seconds 20` (`nitely-cli/src/performance.rs:62`) | count the duration from the first sample and use `--seconds 1`, or call `record(dir, stop)` in process |
| `phone::a_client_trickling_tls_bytes_is_cut_off_at_the_deadline` | 10 | the real 10 s `IO_TIMEOUT` (`http.rs:23`) | `Sock { deadline }` (`phone.rs:334-347`) tested with a past deadline; the timeout a `Phone::start` parameter |
| `mcp_concurrent::a_cancelled_call_is_not_answered_and_frees_the_server` | 6 | a 3 s cap and a 6 s wait for no reply; `run_status` sleeps 1 s (`mcp.rs:1285`) | PURE `Calls` bookkeeping (`mcp.rs:189-235`) |
| `mcp_concurrent::a_call_still_running_when_the_client_leaves_finishes_before_the_server_exits` | 3.3 | 500 ms plus the 3 s cap | MERGE into the next |
| `mcp_concurrent::a_long_call_holds_up_none_after_it_and_ends_at_the_cap` | 3.1 | the 3 s cap | SCENARIO with a 1 s cap; PURE `min(asked, 600, cap)` (`mcp.rs:1273`) |
| `phone::serves_the_page_over_https_with_the_given_certificate` | 3.1 | a 3 s client read timeout | SCENARIO with a 300 ms timeout |
| `mcp_stale::a_call_that_meets_a_newer_database_waits_for_the_install_and_goes_to_the_new_build` | 2.6 | 1.5 s before the install, 750 ms settle, 250 ms polls (`newer.rs:32-50`, `mcp.rs:808-830`) | SCENARIO; signal the install instead of sleeping |
| `mcp_stale::with_nothing_newer_installed_it_refuses_and_says_so_without_waiting_again`, `older_job::with_nothing_newer_installed_the_job_says_why_it_stopped` | 2.1, 2.2 | a 2 s install wait | PURE `install_poll(was, seen, prev, waited_in_vain, now, deadline)` |
| `older_job::a_job_that_meets_a_newer_database_runs_in_the_build_installed_meanwhile` | 3.6 | 1.5 s plus 750 ms | MERGE into the `mcp_stale` scenario |
| executor `machine_transport_runs_snapshot_and_records_executor_without_local_lane` | 2.0 | | see the load-flaky table |

## NIT-537: the long tail

| Test | Win | Decision | Verdict |
| --- | ---: | --- | --- |
| `release_verification::stamped_candidate_full_gate_and_whole_release_review_ignore_pr_switch` | 103 | stamped candidate; base is the last `v*`; Full; the review diffs base to candidate | SCENARIO (its gates wait for a machine slot through `hold_machine`, polling 300 ms) |
| `release_verification::release_without_review_auth_or_with_human_policy_fails_closed` | 57 | missing reviewer fails; `always_human` needs approval | PURE (`review_choice`) |
| `release_verification::failed_attempt_supersedes_previous_pass_and_cannot_release` | 54 | the latest attempt per identity rules | PURE (`verdict(rows, identity)`) |
| `release_verification::dirty_candidate_and_new_policy_invalidate_release_evidence` | 53 | dirty tree refused; a moved policy changes the identity | MERGE the dirty check into the scenario; identity PURE |
| `release_verification::stable_baseline_keeps_earlier_beta_changes_in_the_release_review` | 27 | the baseline tag with `exclude_tags` (git's `describe`) | SCENARIO, `prepare` only |
| `release_verification::failed_full_command_never_gets_a_release_receipt` | 17 | a failing command means no receipt | PURE |
| `release_verification::candidate_cannot_replace_release_policy_or_add_unapproved_stamping_paths` | 6 | | PURE (`check_stamp`) |
| `release_verification::no_release_tag_reviews_from_empty_tree_instead_of_only_last_commit` | 6 | | MERGE into `stable_baseline...` |
| `release_verification::release_only_checks_do_not_replace_missing_full_gate` | 4 | | PURE (`release_commands`) |
| `submodules::a_lane_has_the_submodule_at_its_snapshot_and_keeps_its_build_output` | 26 | the whole submodule follows the snapshot's pointer; kept folders survive | SCENARIO |
| `submodules::a_task_gets_the_submodule_only_when_it_needs_it_and_only_its_cone` | 25 | when a task gets the submodule and its cone | PURE (`needs_submodule`, `cone` is pure) plus one start and widen in the scenario |
| `submodules::a_sparse_lane_switches_pointers_within_its_cone_and_what_the_snapshot_changes` | 20 | the sparse lane cone | PURE (`lane_cone`); git path MERGE into the scenario |
| `submodules::a_pointer_whose_code_cannot_be_shown_still_changes_the_patch` | 16 | | PURE (`patch_parts`, `combine` from `submit.rs:945-979`) |
| `submodules::a_submodule_change_keeps_its_patch_when_the_lines_around_it_change` | 16 | git's `-U0 --submodule=diff` patch id | SCENARIO |
| `snapshot::a_read_after_one_counts_again_only_the_worktrees_that_moved` | 23 | recount only moved worktrees | PURE (`reuse_listing`, `reuse_count`, :231, :262-271) |
| `snapshot::cached_snapshots_match_short_paths_without_confusing_nested_worktrees_or_siblings` | 8 | | PURE (`build` over a hand-made `GitState`) |
| `playbook::starts_a_worktree_and_runs_the_gate` | 11 | | SCENARIO |
| `playbook::a_command_that_fails_then_passes_is_a_flake_and_one_that_keeps_failing_fails` | 6 | | PURE (`classify_runs`) |
| `playbook::finds_the_repo_of_the_issues_project_not_the_callers` | 5 | | PURE (`choose_repo`, `issue_worktree_in`) |
| `playbook::git_keeps_answers_about_object_ids_but_asks_again_about_a_moved_branch` | 4 | | PURE (`settled_query`, `where_query`) |
| `playbook::measures_against_the_newer_of_local_and_remote_base` | 4 | | PURE (`pick_base`) |
| `git::worktree_state_from_the_branch_line_matches_asking_git` | 17 | two git read paths agree | SCENARIO |
| `git::lists_branches_with_counts` | 2 | | MERGE into the above |
| `private_terms::finds_added_lines_paths_and_messages` | 9 | what counts as added | SCENARIO (add an orphan branch to its template) |
| `private_terms::another_projects_list_does_not_apply`, `refuses_a_branch_with_a_term_and_never_echoes_it` | 10, 9 | | PURE (`hits`, `refusal`) |
| `private_terms::scans_a_branch_that_shares_no_history_with_its_base` | 4 | | MERGE into `finds_added...` |
| `install::source_install_serializes_with_land_and_only_delegates_the_exact_holder` | 20 | | PURE (`delegated`, `source_problem`) plus this test as the SCENARIO |
| `install::an_owned_clean_land_stamps_the_outer_build_but_untrusted_or_dirty_contexts_do_not` | 13 | | PURE (`stamp_env`) |
| `install::a_build_older_than_landed_nitely_code_is_stale` | 4 | | PURE (message from the count) |
| `overlap::next_task_reads_held_files_from_branches_and_says_why` | 5 | | SCENARIO (the only test of `held`'s reads) |
| `release_notes::keys_titles_and_the_commits_that_name_none` (crew) | 3 | | PURE (`entries_from_log`) |
| `test_repo::a_copy_is_its_own_repo_with_its_own_remote` | 10 | | PURE (`re_root`, `test_repo.rs:58`) |

## NIT-527: what the templates must serve

After the cards above, the scenarios left are the ones named in each section. NIT-527 builds one
template per kind of fixture they use (plain repo with a bare remote; repo with a submodule; JS
and Cargo selector repos; the release fixture; the large-repo lane fixture), built once per test
binary and cloned cheaply, and measures each scenario's setup before and after.

Done (NIT-527): `test_repo::init` is itself a template keyed by its files, built once per test
binary, so every test that calls it (45 at the time) copies a finished repo instead of starting
`git init`, `add` and `commit`; `copy` and `init` hardlink git's object files, which git never
changes in place, and copy the rest. A folder that already holds files is still set up in place.
Measured with `nitely run` on Windows, the same 23 tests (snapshot, submodules, blobs, review_batch,
private_terms, batches, test_repo, one playbook test): 5.74 s before, 5.06 s after; per test,
`snapshot::a_read_after_one_counts_again_only_the_worktrees_that_moved` 1.32 to 0.68 s, the
submodule scenarios about 0.4 s each less, `private_terms` 0.4 s each less. What remains in each
scenario is the git its own steps start, which NIT-518's in-process reads cut.

## Expected result

Run 1666's 245 tests over 2 s become about 50 named scenarios and their PURE replacements, which
run in milliseconds. The remaining Windows cost is the scenarios' git starts, which NIT-518 (git
in process) and NIT-527 (templates) cut. No figure is promised until each card measures its own
module with `nitely run` before and after, the way docs/testing.md records NIT-498's.
