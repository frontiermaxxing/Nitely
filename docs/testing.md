# Tests that earn their cost

A test is worth keeping when it detects a plausible mistake that matters to someone using the product. Before adding one, describe the trigger, the wrong observable result, and why an existing test would miss it. A changed file, a review checklist, a coverage percentage, or a target test count is not a reason by itself.

Write tests for a reported bug's regression, a public API or protocol contract, a tricky boundary or state transition, and behavior spanning components. Prefer a small realistic scenario with the actual boundary involved: a temporary repository for Git behavior, a temporary database for persistence, or a request through the public interface. Use fakes for an external system that would make the test unsafe, slow, or nondeterministic. Do not mock the logic being tested and then assert that the mock was called as configured.

One useful test is better than five variations that restate the same implementation. Assert the outcome a caller needs, including important failures and durable side effects. Private helper names, incidental ordering, exact internal call sequences, and large snapshots of unrelated output make refactoring expensive without improving confidence. Snapshots are useful only when their contents are the contract and a person can review the change meaningfully.

A regression test should fail because of the reported defect, then pass because of the fix. Do not manufacture a broad fixture or launch a native application when a focused test can exercise that same failure. For display copy or a reversible visual adjustment, inspecting the result may be sufficient. Record what was checked and its limits.

Reviewers may flag a missing test only when they can name a concrete, likely regression: its trigger, wrong outcome, and the gap in existing coverage. A lack of newly added tests is not itself a defect. The shared five-line standard is included in every connected-agent workflow and both the single-change and batch review prompts.

Keep verification proportional. Use a build lane, reuse already collected evidence for the same revision, and run the narrowest check that answers the unresolved question. A passing check is evidence only for what it exercised. Follow the project's configured gate and the person's explicit verification choices; this standard does not silently change either.

Known failures are work to resolve, not a permanent success condition. A failure suppressed in a known-failure list must remain visible as a failure or exception, with its owner and reason. If a test is obsolete or duplicates better coverage, propose its removal and explain what still protects the behavior. Do not silently delete failing tests or weaken assertions to make a gate green.

A hygiene sweep should report the suite revision, commands, run count, outcomes, individual timing coverage, and uncertainty. Classify repeated failures, inconsistent outcomes, slow contributors, duplicated cases, and possible implementation coupling. Similarity and mocking are review leads, not proof that a test is worthless. Present one editable decision with precise candidates and evidence; after approval, apply changes on a branch through normal submission and landing. Compare counts and warm-lane timing before and after; never invent savings from an unmeasured deletion.


Use `/nitely-test-sweep` in connected Claude projects, `test_sweep_guide` through MCP, or
`nitely test-sweep guide`. `collect --logs logs.json --output report.json` is report-only:
logs carry their real revision and distinct run ID. `--execute` explicitly requests a bounded
lane run (two repetitions, ten minutes by default; at most three and one hour). Node TAP supplies
per-test timings; `--rust-timings` measures stable Rust test binaries individually and includes
process startup cost. Neither is an estimate of a test's contribution to a parallel suite.
`--known-failures names.json` highlights observed failures on an explicit list; it never calls them passing.

Reports retain outcomes, source body hashes, revision, evidence sources, timing coverage and
truncation. Source extraction is conservative, not a complete test count or an AST proof of
redundancy. `test-sweep propose` groups the reviewed candidates into editable action lists in
the Inbox. Inspection alone does not authorize removal. Approving files normal branch tasks;
changed source evidence must be reassessed before deleting anything. Existing logs avoid a
new suite run, and an empty report needs only a handoff with its limits.

A running Crew can opt into `[schedule] test_sweep = "nightly"` or `"weekly"` (UTC periods;
default `"off"`). It creates one report task per period, respects Crew's start/pause limits,
and avoids another while a sweep or its proposal is open. No schedule approves a proposal.


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
