# Periodic integration reviews

Periodic review reads accumulated changes after delivery. A project's human-owned review mode
sets its landing threshold, optional maximum delay, reviewer and finding strategy. It is separate
from pre-merge review, reduced-check audits and the Full test backstop. Turning pre-merge review
off does not turn periodic review off. YOLO does disable automatic periodic review.

The first enabled observation establishes a **baseline**, not a successful review certificate.
The last successfully reviewed commit remains null until a reviewer completes. From that baseline,
each distinct issue/head landing counts once; a batch counts its individual landed changes.
Replayed completion events do not increase the count. The successful landing path wakes a detached
worker at the threshold. The desktop checks once a minute for the delay and unfinished jobs.
A delay of zero disables the time trigger. Changes made outside Nitely, or a crash between Git's
ref update and its database event, are still detected by the time trigger without inventing an
issue count. With the time trigger disabled, those changes wait for the next qualifying landing batch.

Each repository is identified by its canonical common Git directory. A process lock prevents two
workers reviewing it concurrently; a durable job fixes the base and head. The reviewer gets a
separate detached source checkout at that head and the existing process limits and memory admission.
Reviewer configuration comes from the integration branch's committed playbook, or the connected
repository's stored playbook when none is committed; app defaults fill unset fields. Uncommitted
checkout files cannot override that authority. Human mode authority, repository registration,
policy contents and app defaults are reread immediately before launch, including after memory waits.
Ordinary new landings with the same policy do not invalidate the fixed range.

A completed structured review is retained before finding actions start. Its result is bounded to
1 MiB and 50 findings. Interrupted actions resume without rerunning the review. The cursor advances
only after those actions are recorded, and new landings beyond the reviewed head stay pending.
Failures keep the cursor and evidence, retry at most three times with increasing delays, then
remain visibly failed. A dead worker's final attempt becomes failed too, so it can be retried.
Changing modes while a review runs retains its result but defers new actions while periodic review
is disabled. Integration history rewrites stop the worker instead of guessing a new baseline.

Findings carry the reviewer’s priority and explanation. Under `reopen_or_create`, Nitely reopens a
Done source issue only when the finding explicitly names exactly one issue from this range,
main still equals the reviewed head, and no human closure/decision or accepted finding would be
overridden. Otherwise it files a linked follow-up that tells its next agent to confirm applicability
against current main. `create` always uses follow-ups. Identical normalized findings in the same
repository reuse existing **open** work. A new occurrence after its previous card closed gets an
actionable follow-up; it is not silently swallowed. Same-job markers make issue creation and
reopening recoverable after a crash between the card action and its bookkeeping.

This reviewer never reverts main, blocks a land or grants an approval. Existing signed decisions,
original review evidence and issue history stay intact.

Agents can read retained status with `nitely periodic-review --project APP`; `--tick` runs one due
review for that project's configured repositories. Read APIs return fixed repository identities,
the baseline and reviewed cursor, pending counts, errors and the ten newest jobs with linked
finding actions. Missing or failed reviews never appear as passes.
An explicitly requested retry uses `nitely periodic-review --project APP --retry 17 --repo <identity>`
with the repository identity from status. It resets the failed attempt count, not the cursor or
settings, and cannot make a disabled reviewer run.
