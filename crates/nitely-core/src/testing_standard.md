Write a test when it protects a concrete bug regression, public contract, tricky edge case, or behavior across components.
Prefer a realistic scenario or integration test; one useful case beats several tests that restate the implementation.
Do not add tests just because a file changed, to meet a count, or to pin private details, mock interactions, or incidental snapshots.
Do not normalize known failures: repair them, or propose removing redundant or obsolete coverage with evidence and a human decision.
A reviewer may request a missing test only for a concrete, likely regression: name the trigger, wrong outcome, and the gap in existing coverage.
