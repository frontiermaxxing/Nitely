# Auditing reduced verification

[Audit sampling](http://127.0.0.1:7423/i/NIT-302) records one random selection per
issue and authoritative verification policy. Its default is 10%; accepted policies
may request 10–100%. A server-generated 256-bit project salt and a SHA-256 draw
determine selection. The salt never appears in the read model. The stored decision
survives declaration edits, revised candidates, restarts and project renames.
The policy fingerprint hashes committed policy contents and the classifier identity,
not the integration commit, so unrelated ancestor changes do not reroll the sample.
The current classifier remains shadow-only: this implementation does not activate a
reduced gate or skip pre-land review. Shadow candidates receive durable sampling
advice; only an actual reduced landing enters the execution queue.

Every actual reduced landing is recorded with its exact base/head, implementation
vendor, declaration provenance, effective risk and stable lowering-rule identities.
Unsampled landings remain in the denominator. Repeating the same landing is
idempotent; a later distinct landing of a reopened issue is another observation using
the existing policy's sample. Shadow advice must not be reported as a reduced landing.

Selected landings enter a durable queue. The worker claims one job transactionally,
then releases the database lock before preparing a dedicated checkout and running a
full independent peer review of the entire landed diff. The checkout must be clean
at that exact commit and in the recorded repository. Repositories sharing one project
cannot consume each other's jobs. Existing reviewer policy and process timeouts remain in force.
Missing reviewers, disabled review, incomplete replies and process failures are
recorded as failures, never passes. Infrastructure failures require an explicit retry.
A killed worker's three-hour lease can be reclaimed; attempt fencing rejects any
late result from the previous worker. A recovered read-only review can repeat work,
but cannot replace its successor's result or mutate the landed branch.

A reviewer finding is not automatically a confirmed escape. Human confirmation
records evidence against a rule that actually lowered the landed change. The
post-merge backstop can also confirm an escape after reproducing a failure and
isolating that exact culprit. Confirmation immediately disables that lowering rule
for the project in Nitely's database. Classification consults this untruncated set
and returns that scope to Full. No agent edits the live playbook. Re-enabling a scope
requires a revised rule, regression evidence, shadow evaluation and the separately
approved policy process; this API offers no agent-accessible reset.

The read model returns at most 200 jobs, rule groups, declarer/source groups and
disabled-rule names, with explicit truncation flags. Counts cover all retained
landings, including unaudited ones. Escape counts count affected landings once even
when multiple confirmed findings exist. The audited escape rate is absent before
any completed audit, and excludes unaudited post-merge escapes from its numerator;
those remain visible in total confirmed-escape counts. Declaration matches compare
the declared level with the mechanically effective level, not with an inferred
notion of safety. A matching declaration can still have a confirmed escape.

Integration hooks live in `verification_audit`: `record_candidate`, `landed`,
`run_next` with `full_review`, `confirm_escape`, `disabled_rules`, and `report`.
The executor creates/removes the isolated checkout and schedules the worker through
the existing resource-controlled job infrastructure. Backstop callers use the
internal `confirmed_culprit` hook only after isolation. Sampling alone does not
authorize reduced verification or waive approval, release, or backstop requirements.
