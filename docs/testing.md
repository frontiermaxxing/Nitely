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
