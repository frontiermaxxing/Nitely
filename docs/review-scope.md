# Reviewing planned work

A review asks which contract this particular change must deliver, who consumes it and when the assembled path must work. A producer can pass while a named consumer is still open. A broken interface or permission bypass introduced by the producer still blocks it.

Nitely stages a private temporary directory for the reviewer. The first prompt includes an index excerpt. The full index lists the current issue, direct prerequisites and consumers, explicitly referenced same-project issues, and goal siblings. Numbered files contain their specification, evidence, submitted commit when available, questions and answers, and attributed notes. The goal and checkpoint have their own identities. A reviewer can read these files, the integration base and any available recorded commit using its existing read-only tools. There is no new board connection, and reviewing never waits for a sibling. A note is a claim to check; it cannot supersede a human decision or sign an acceptance.

Limits: up to 128 source issues, 32 recent note/decision events and 16 answers per source, 65,536 characters per text field and a 4 MiB collection threshold. Truncation or missing records are explicit. Missing scope is unknown, not permission to defer. The files are removed when the review finishes normally. The database keeps the reviewer's scope interpretation, obligation and decision alongside the code revision, source identities, authority and plan fingerprint. Historical statements are evidence to recheck, not transferable approvals.

Individual, batch and dispute reviews can consult the evidence in the same invocation. A relevant plan change invalidates reuse without requiring a code commit. Checkpoints review the assembled slice; release review consults obligations recorded for that repository in the release verifier's database. A release machine without board history explicitly reports that limitation and still reviews the whole release diff.

A scoped reviewer can identify a specific unresolved choice. Repeated rounds alone no longer create a request to accept each finding under the Ask policy. A confirmed defect remains blocked with a repair assigned; a product question does not waive a risk. Explicit signed risk acceptance, independent dispute checks and the configured split-and-fix policy remain separate.

## Measure the outcome

Run [review-scope-metrics.sql](../scripts/review-scope-metrics.sql) read-only, binding the project key and a UTC start time. It returns one row per landed issue: elapsed seconds from first worktree to land (null if unknown), review rounds, same-commit rounds with intervening clarification evidence, and carried or exactly repeated findings. The clarification count is a proxy: inspect the notes before attributing a round to missing context. Compare similar changes before and after rollout and inspect escaped integration defects and security blockers separately; fewer findings alone are not success.
