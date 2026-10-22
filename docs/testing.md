# Tests that earn their cost

A test is worth keeping when it detects a plausible mistake that matters to someone using the product. Before adding one, describe the trigger, the wrong observable result, and why an existing test would miss it. A changed file, a review checklist, a coverage percentage, or a target test count is not a reason by itself.

Write tests for a reported bug's regression, a public API or protocol contract, a tricky boundary or state transition, and behavior spanning components. Prefer a small realistic scenario with the actual boundary involved: a temporary repository for Git behavior, a temporary database for persistence, or a request through the public interface. Use fakes for an external system that would make the test unsafe, slow, or nondeterministic. Do not mock the logic being tested and then assert that the mock was called as configured.

One useful test is better than five variations that restate the same implementation. Assert the outcome a caller needs, including important failures and durable side effects. Private helper names, incidental ordering, exact internal call sequences, and large snapshots of unrelated output make refactoring expensive without improving confidence. Snapshots are useful only when their contents are the contract and a person can review the change meaningfully.

A regression test should fail because of the reported defect, then pass because of the fix. Do not manufacture a broad fixture or launch a native application when a focused test can exercise that same failure. For display copy or a reversible visual adjustment, inspecting the result may be sufficient. Record what was checked and its limits.

Reviewers may flag a missing test only when they can name a concrete, likely regression: its trigger, wrong outcome, and the gap in existing coverage. A lack of newly added tests is not itself a defect. The shared five-line standard is included in every connected-agent workflow and both the single-change and batch review prompts.

Keep verification proportional. Use a build lane, reuse already collected evidence for the same revision, and run the narrowest check that answers the unresolved question. A passing check is evidence only for what it exercised. Follow the project's configured gate and the person's explicit verification choices; this standard does not silently change either.

Known failures are work to resolve, not a permanent success condition. A failure suppressed in a known-failure list must remain visible as a failure or exception, with its owner and reason. If a test is obsolete or duplicates better coverage, propose its removal and explain what still protects the behavior. Do not silently delete failing tests or weaken assertions to make a gate green.

A hygiene sweep should report the suite revision, commands, run count, outcomes, individual timing coverage, and uncertainty. Classify repeated failures, inconsistent outcomes, slow contributors, duplicated cases, and possible implementation coupling. Similarity and mocking are review leads, not proof that a test is worthless. Present one editable decision with precise candidates and evidence; after approval, apply changes on a branch through normal submission and landing. Compare counts and warm-lane timing before and after; never invent savings from an unmeasured deletion.


## Optimizing any repo's tests

`/nitely-test-optimize` (the skill `connect` installs; `test_optimize_guide` through MCP;
`nitely test-sweep guide`) runs one method on any connected repo, in any language:

1. **Measure without the person's machine.** Existing CI logs and JUnit XML first, the format
   most runners write (pytest, jest, vitest, Go through gotestsum, cargo-nextest, .NET, Maven,
   Gradle); Rust's libtest and Node's TAP are read too. Then, when there are none, one CI run
   dispatched under a cost cap in minutes, shown before it starts. Only a repo without CI gets a
   capped build-lane run. Two runs at one revision tell a flaky test from a failing one. Each
   time keeps its method, and times are compared within one method only. A cut or unreadable
   file is a partial run: what it did not report is unknown, never passing.
2. **Flag what to look at.** Tests slower than the budget's per-test limit, flaky or failing ones,
   bodies that wait on the clock or assert UI text or presence, bodies the same as another's, and
   tests whose gate failures in that project were their own more than once and never caught
   anything (the fix changed only tests).
3. **Classify each flagged test** by reading it: the decision it checks and the module that makes
   it, a scenario that proves real I/O, a presence test, a wall-clock wait, a duplicate, a
   restatement of the implementation, or one worth keeping as it is, with why.
4. **Propose one goal** the person approves: a refactor card per module (the decisions to lift
   into pure functions, the scenarios kept, the time measured), a card for clock waits, a time
   budget naming the scenarios, one deletion card after the refactors that names what covers
   each test, and, where the repo has CI, a playbook change that sends every test command there.
   Nothing is deleted or refactored before that approval, and the deletions take one more.

The rules every proposal follows: time is an input; no presence tests; one or two end-to-end
scenarios per feature, with the decisions inside them as pure functions; full suites in CI, never
on the person's machine; one approval for the whole deletion list. Measure before and after, and
never claim a saving that was not measured.

A running Crew can opt into the light mode, `[schedule] test_health = "nightly"` or `"weekly"`
(UTC periods; default `"off"`; `test_sweep`, its old name, still works): one report-only health
check per period, which respects Crew's start and pause limits and is not filed again while one
is open. No schedule approves a proposal.


## The optimizer on Nitely itself (NIT-559)

The first run of the method, on Nitely, 2026-10-22: a baseline before any outside repo. `measure`
found no CI logs at the goal branch's head and read the 5 CI logs of `eae9d0e`, the newest main
commit CI had tested. `collect` turned them into 2,927 observations, inventoried 958 test bodies,
and found 11 collector leads.

`map` flagged 88 tests: 78 only for "waits on time or load", 8 for UI text or presence, and 5 as
slow. Read against the code, most of the waits and UI leads were the classifier's, not the tests':

- The waits hint counted `Duration::from_secs`, `from_millis` and the word `timeout`. Those are
  values passed to a function, and goal 20's pure tests pass them on purpose. The hint now counts
  only calls that block or poll (`sleep`, `setTimeout`, `wait_for`, `recv_timeout`, ...). On the
  same tree, 38 tests carry it instead of 78. The 38 sleep, poll or block for real: installs over a
  running binary, lock and lane timeouts, a TLS deadline, the MCP server's call caps.
- The UI hint counted the bare word `html`, which named files and JSON fields. It now counts `x in
  html`, a page's text asserted as such. 2 tests carry it instead of 11.
- The 2 left are fixtures: tests written inside string literals in the collector's own test file,
  which the inventory reads as tests ([NIT-594](http://127.0.0.1:7423/i/NIT-594)).

The 5 slow tests are the real-window scenarios in CI, which are named and kept.

One real gap: CI's evidence carried no Rust test times. The push `gate` job runs `cargo test`
without the runner that adds `--report-time`; only the dispatched `suite` job has it. So no slow
Rust test could be flagged, and a proposal from this evidence would be blind to them
([NIT-593](http://127.0.0.1:7423/i/NIT-593)). No goal was proposed from this run. Run it again once
NIT-593 has landed, then classify what it flags and propose.

## First Nitely sweep: retain contracts, reduce setup

The first approved cleanup, [NIT-401](http://127.0.0.1:7423/i/NIT-401), follows
[NIT-306](http://127.0.0.1:7423/i/NIT-306). Historical gate 1306 had flagged three
scenarios running for over 60 seconds under parallel suite load. That was a lead,
not an individual timing or proof of a flaky test. No deletion was proposed.

The bisection fixture now starts with explicit reviews of its actual Git patches,
so it does not run five unrelated submit pipelines before testing the queue. It
still uses real branches, failing gates, bisection and landing, and checks that
only the good members' files reach main and their issues become Done. The
three-member variant shares this fixture too. Other queue integration tests still
exercise the real submit-to-land boundary.

The disabled-review scenario submits one real change under all four policy modes
instead of rebuilding four repositories. Every mode still runs its gate and checks
neutral review evidence and no queued review. One mode exercises the remote push;
mode-independent approval checks run once, including refusal when review is enabled
again. A changed patch is refused, then the original patch actually lands. The
summary scenario keeps its four real submissions and gates, but explicitly skips
the fake reviewer: it checks revision binding, stale queued text, synchronous text,
and missing legacy text, and asserts that text does not manufacture a review pass.

Measurement uses three serial `cargo test -p nitely-core --lib <full-test-name> --
--exact --test-threads=1` commands in warm Windows build lanes. Rust's test elapsed
time excludes Cargo compilation and lane queue/setup. The baseline is
`9ab55bd303115fcf286f612a7a23544250bdbad1`, lane run 1338 in lane 1. Run 1342 in the
same lane measured the final bisection and summary bodies (snapshot `09bd0ae`).
Run 1344 in lane 2 measured the final disabled-review body (snapshot `7a444fb`).

| Exact Rust test | Before | After |
| --- | ---: | ---: |
| `land_queue::tests::a_bad_change_is_bisected_out_of_five` | 59.41s | 38.35s |
| `submit::tests::disabled_review_keeps_gate_and_landing_without_synthetic_review_or_fallback` | 29.82s | 26.03s |
| `submit_job::tests::solution_summary_is_bound_to_the_revision_handed_in` | 27.39s | 13.87s |
| Sum of these observations | 116.62s | 78.25s |

All three pass before and after. The core binary still registers 610 tests; zero
tests were added, deleted or merged. The three-member bisection variant also
passes with the shared fixture. The first disabled-review edit took 49.32s; its
new real land exposed the cost of redundant per-mode remote/approval work, which
the final edit removes. The final scenario covers more behavior than the original.

The observed reduction is 38.37s (about 33%) for these three serial scenarios,
not for the whole suite. These are one observation per body version on a shared
machine; other work and cache state were not controlled, and the final matrix
measurement used a different warm lane. No new whole-suite test count, warm-suite
time, or flake rate was measured. Historical gate 1306 had 632 passing Rust tests,
five ignored Rust tests, and 165 passing Node tests on an older source revision;
those counts must not be presented as today's suite or mixed into this comparison.


## Test diet: seed what a test is not about (NIT-498)

[NIT-498](http://127.0.0.1:7423/i/NIT-498) worked through the slowest core tests
after [NIT-458](http://127.0.0.1:7423/i/NIT-458), which found that git process
starts are about 83% of a land or submit test.

What changed, with no check dropped:

- **Seeded submissions.** `test_repo::reviewed` records what a submit that passed
  leaves for land: a pass review of the branch's real patch at its head, and the
  issue In Review. Land and queue tests about landing use it. Tests about submit
  itself, approval reasons, or project-mode evidence still run a real submit, and
  `lands_reviewed_work_and_waits_for_human_approval` and the queue's
  lands-together test keep the real submit-to-land path.
- **Seeded review rounds.** Finding, request and round tests record the
  reviewer's findings or earlier rounds directly. They no longer rewrite the
  fake reviewer and submit once per round.
- **Plain functions.** The dependency-file warning calls `after_land` on a plain
  repo instead of two lands. slim's "an agent is at work here" rule is a
  function with its own test. `review_context` is read from the database alone.
- **Merged scenarios**, each one fixture and fewer lands or queue runs:
  - the kept, process-held and linked worktree tests: one tidying land instead
    of three;
  - a changed patch needing a new approval, inside the signed-approval test;
  - author mode, inside the human mode test;
  - lands-together, conflict-in-the-middle and moved-since-review: one queue run
    instead of three.

Three deletions were put to the person (ask 68 on NIT-498), approved, and applied
after the measurements below, so they are not in them: the bisection of three
(the bisection of five runs the same helper), the no-origin land at the end of
the failed-push submit test (every land.rs land has no origin; the pushed-is-none
check moved there), and the last two synchronous submits of the solution-summary
job test.

**Measurements.** Whole core suite, `cargo test -p nitely-core --lib --
--test-threads=12` with `--report-time`, warm lane 1, once at the start and once
at the end:

| | Before (run 1601, a1f4241) | After (run 1666, e67b215) |
| --- | ---: | ---: |
| Wall time | 818 s | 449 s |
| Summed test time | 9,585 s | 5,320 s |
| Tests passed / failed / ignored | 654 / 1 / 5 | 658 / 0 / 6 |
| Changed modules, summed (land, land_queue, submit, submit_job, slim) | 6,541 s | 3,193 s |
| Untouched modules, summed | 3,044 s | 2,127 s |

The PC was shared with other agents' lanes, and the load was lighter at the end:
the modules this work did not touch also fell by 30%. Against that, the changed
modules fell by 51%, so about 30% is this work and the rest is load. Individual
module runs of the same tests varied up to twofold within an hour.

A load-independent count agrees. Every git process the changed modules start,
counted per test with a temporary trace in `run_git` (runs 1663 and 1665, same
filter, baseline and branch):

| Module | Git processes before | After |
| --- | ---: | ---: |
| land_queue | 3,359 | 2,283 |
| land | 2,875 | 1,922 |
| submit | 1,753 | 1,536 |
| submit_job | 267 | 252 |
| slim | 150 | 123 |
| Total | 8,542 | 6,251 (-27%) |

The 2-minute target was not reached, and test changes alone will not reach it.
A seeded five-change bisection test still starts 249 git processes, and about 90%
of them come from land, land_queue and lanes code: 30 `land::rev` calls, 18
checkouts in the stack worktree, and 15 patch-id ranges. Every real land pays the
same cost. Cutting those calls is
[NIT-508](http://127.0.0.1:7423/i/NIT-508). The between-lane timings above are
single observations; the base commits differ by other goal-19 work, which added
tests and fixed the one failure. This work removed six tests by merging and added
two plain-function tests.


## The time budget (NIT-522)

[NIT-522](http://127.0.0.1:7423/i/NIT-522) gives every gate each test's time and holds the
suite to `.nitely/test-budget.toml`:

- `test_seconds`: what any one test may take (2 s);
- `[suites]`: what a whole test binary may take, by the name cargo prints for it
  (`nitely_core = 60`), or `node` for node:test;
- `[scenarios]`: the end-to-end tests allowed past `test_seconds`, by name, each with what it
  proves. One or two per feature; [NIT-523](http://127.0.0.1:7423/i/NIT-523) names them.

**How the times are taken.** Stable Rust prints no per-test times. In a build lane, a command
that runs `cargo test` gets `nitely test-runner` as cargo's test runner
(`CARGO_TARGET_<triple>_RUNNER`). It starts each test binary with `-Z unstable-options
--report-time` and `RUSTC_BOOTSTRAP=1`, which only the test binary sees, so the build is the one
without it and a warm lane stays warm. A repo with a test harness or runner of its own
(`harness = false`, a `runner` in `.cargo/config.toml`), or a command that sets `--format`, runs
as before, without times. In CI, the suite job in `.github/workflows/ci.yml` writes the same
runner into the job and sets it for Linux, and so does the push gate job's Linux run, whose log
`test-sweep measure` reads first (NIT-593). node:test's TAP output has each test's `duration_ms`
already. A report names each source whose tests carry no time, since none of those can be found
slow.

**What the gate says.** `test_budget.rs` reads the times from the lane's log, or from the CI
job's whole log (`gh run view --log`), and adds two lines to the gate's warnings:

- `Test time, N tests timed: nitely_core 31 s (target 60 s), ...; slowest: ...` on every gate
  that ran tests;
- `Over the test budget in .nitely/test-budget.toml: ...` when a suite is over its target or a
  test that is not a named scenario is over `test_seconds`. It names the ten slowest and counts
  the rest, and goes on the issue's timeline as a `test_budget` event.

A broken budget is flagged, not failed, in every review mode for now: the suite is far over it
on Windows (below), and failing every gate would stop all work until goal 20 is done. Making it
fail under Careful and Release once the suite meets the budget is a follow-up.

### Baseline, 2026-10-21

The same tests, on two machines:

| | CI, Linux (GitHub `ubuntu-latest`, 4 cores) | Build lane, Windows (this PC) |
| --- | ---: | ---: |
| Source | main `11f1516`, workflow run 39638986501 (552 tests) | `e67b215`, lane run 1666, 2026-10-20 (658 tests: the crew's were still in core) |
| Command | `cargo test --workspace -- --test-threads=4` | `cargo test -p nitely-core --lib -- --test-threads=12` |
| `nitely_core` wall time | **31 s** | **449 s** |
| Summed test time, `nitely_core` | 123 s | 5,320 s |
| `nitely_core` tests over 2 s | 13 | 245 |
| over 10 s | 1 | 129 |
| over 60 s | 0 | 28 |

On Linux the core suite is already inside its 60 s target. On Windows it is 7.5 times over,
and the summed test time is 43 times Linux's. The tests are the same; what differs is the cost
of starting a process, which NIT-458 measured at about 83% of a land or submit test, and
[NIT-518](http://127.0.0.1:7423/i/NIT-518) measured at 600 to 900 ms per git start on this PC.
So the pure-function cut (NIT-523 to 527) pays most where processes are slow: every git call a
test no longer makes is worth up to a second there.

The CI run stopped after `nitely_core`, which had 10 failing `release_verification` tests and
one `lanes` test on main at the time. Cargo stops at the first failing test binary, so
`nitely_crew` and the leaderboard service are not in it. The other binaries that ran:

| Binary | Wall time |
| --- | ---: |
| `performance_recording` | 20.2 s (one test: `performance_record_duration_stops_cleanly`, 20.1 s) |
| `mcp_concurrent` | 6.2 s |
| `older_job` | 3.6 s |
| `mcp_stale` | 2.6 s |
| `executor` | 2.0 s |
| everything else | under 2 s each |

The Linux tests over 2 s, which are the first list for NIT-523:

| Test | Linux |
| --- | ---: |
| `performance_recording::performance_record_duration_stops_cleanly` | 20.1 s |
| `phone::tests::a_client_trickling_tls_bytes_is_cut_off_at_the_deadline` | 10.2 s |
| `mcp_concurrent::a_cancelled_call_is_not_answered_and_frees_the_server` | 6.1 s |
| `db::tests::a_write_waits_out_a_long_write_of_another_connection` | 6.0 s |
| `performance::tests::a_busy_database_does_not_lose_the_new_collectors_version_announcement` | 5.6 s |
| `lanes::tests::a_memory_reason_goes_once_a_request_waits_for_the_checkout` | 4.1 s |
| `lanes::tests::a_floor_lowered_while_a_request_waits_lets_it_go` | 4.1 s |
| `lanes::tests::a_request_waits_for_memory_and_says_so` | 4.1 s |
| `machine::tests::admit_waits_until_memory_frees_and_records_the_start` | 4.0 s |
| `older_job::a_job_that_meets_a_newer_database_runs_in_the_build_installed_meanwhile` | 3.6 s |
| `submit::tests::a_p1_that_two_rounds_leave_open_goes_to_a_person` | 3.4 s |
| `land::tests::lands_only_with_the_apps_signed_approval_of_the_patch_it_saw` | 3.4 s |
| `mcp_concurrent::a_call_still_running_when_the_client_leaves_finishes_before_the_server_exits` | 3.3 s |
| `phone::tests::serves_the_page_over_https_with_the_given_certificate` | 3.1 s |
| `mcp_concurrent::a_long_call_holds_up_none_after_it_and_ends_at_the_cap` | 3.1 s |
| `mcp_stale::a_call_that_meets_a_newer_database_waits_for_the_install_and_goes_to_the_new_build` | 2.6 s |
| `goal_branch::tests::items_land_into_the_goal_branch_and_a_checkpoint_lands_the_slice_on_main` | 2.6 s |
| `lanes::tests::a_memory_reason_goes_once_a_request_waits_for_a_slot` | 2.4 s |
| `older_job::with_nothing_newer_installed_the_job_says_why_it_stopped` | 2.2 s |
| `lanes::tests::a_request_waits_while_the_main_checkout_has_changes_and_says_why` | 2.1 s |
| `mcp_stale::with_nothing_newer_installed_it_refuses_and_says_so_without_waiting_again` | 2.1 s |

Most of these wait on the clock (a deadline, a poll interval, memory that frees later), which
the goal counts as a defect. On Windows the slowest are the git-heavy modules instead, by
summed time: `land_queue` 1,039 s, `submit` 1,037 s, `land` 815 s, `lanes` 428 s,
`release_verification` 328 s, `crew` 264 s, `submit_job` 233 s and `affected_tests` 138 s.
These are single observations: run 1666 shared the PC with other agents' lanes, and a CI
runner's speed varies from run to run.


### After goal 20's cut, 2026-10-21 (NIT-528)

Andrew approved the deletion list (ask 69: "Approve all three groups"). The old tests that pure
tests covered are gone: 47 deleted, 5 merged into the scenarios that keep their checks, and the
bisection scenario cut from five changes to three. Every deleted test and what covers it is listed
in docs/pure-cut.md, "NIT-528: the deletion list". The kept scenarios are named in
`.nitely/test-budget.toml`.

The same CI measure as the baseline (GitHub `ubuntu-latest`, 4 threads), on NIT-528's tree,
workflow run 39681612807:

| | Baseline (run 39638986501) | After (run 39681612807) |
| --- | ---: | ---: |
| `nitely_core` wall time | 31 s | **18.9 s** (target 60 s) |
| `nitely_core` summed test time | 123 s | 75 s |
| `nitely_core` tests | 552 | 564 |
| `nitely_core` tests over 2 s | 13 | 5 |
| Workspace tests over 2 s | 21 | 10 |
| `performance_recording` | 20.2 s | 1.4 s |
| `mcp_concurrent` | 6.2 s | 3.1 s |

The core suite has more tests than before: the pure tests added since the baseline outnumber the
old tests removed. The wall time and the tests over 2 s fell all the same. On Windows the change is
larger, since the cut removed the git starts that cost 0.6 to 0.9 s each there. It was not
measured with a full local run (the push runs full suites only in CI); each gate's "Test time"
line shows it per lane run.

Ten tests are still over 2 s. Four are named scenarios: `goal_branch::...items_land_into_the_goal_branch...`
(2.6 s), `mcp_stale::a_call_that_meets_a_newer_database...` (2.5 s),
`mcp_concurrent::a_long_call_holds_up_none...` (2.4 s), and `submit::...a_p1_that_two_rounds...`
(4.2 s). The others, and what takes them under:

- `land::...lands_only_with_the_apps_signed_approval...` (3.3 s), and the scenario above: 1.1 s
  sleeps because review and approval times are whole seconds,
  [NIT-544](http://127.0.0.1:7423/i/NIT-544).
- `mcp_concurrent::a_cancelled_call_is_not_answered...` (3.2 s): it waits out the 1 s cap and then
  asserts silence; the bookkeeping is pure since NIT-536, so the wait could be shorter.
- `submit::...the_fresh_review_is_told...` (2.8 s) and `submit::...the_same_edit_somewhere_else...`
  (2.4 s): kept on purpose (what the reviewer is told; `diff_id` against real git).
- `install_and_forward::a_running_mcp_server_forwards...` (2.7 s) and
  `install_and_forward::install_from_a_file_beside...` (2.2 s): real installs of the binary.
