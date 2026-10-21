# Post-merge verification

The backstop is opt-in committed integration policy. The desktop checks once a minute;
headless installations can run `nitely verification --tick` from their scheduler. Both
use one repository lock and durable records, so they do not duplicate a check. The Full
gate, and the runs that bisect a failure, go to GitHub Actions through the CI executor
(docs/build-lanes.md, "Full suites in GitHub Actions"): every gate command, at the exact
commit. When CI cannot take one (this machine's `executors.json` has no `ci_projects` for
the project, the origin is not on GitHub, gh is not signed in, the dispatch is refused, or a
run never reports), it runs in the project's warm lanes under the existing machine
admission limits, as before.

Every preset keeps the backstop on, with automatic revert (NIT-496): Fast and Careful check
after five landings or 30 minutes, Release after every land. Only Custom turns it off, and
"How a change lands" shows that in red.

This is a proposal example, not an active Nitely policy change:

```toml
[verification]
version = 1
mode = "shadow"
audit_percent = 10

[verification.backstop]
enabled = true
landings = 5
delay_seconds = 1800
nightly_seconds = 86400
auto_revert = true
reversible_paths = ['docs/help/.*\.md', 'src/components/.*\.(ts|tsx)']
irreversible_paths = ['src/components/Payment.*']
```

The first run establishes a Full baseline. Later checks run after five reduced landings,
30 minutes after the first unverified reduced landing, or a day after the last Full pass
when integration changed. Every configured gate command runs, including conditional steps.
Past successful results cannot satisfy a fresh backstop run. Unknown policy, empty gates,
missing lanes or unavailable memory cannot produce a green result.

The current rollout only records classification advice: it does not reduce submit or land
gates. The reduced-land counter records actual omitted Full checks, not shadow advice.
Nightly checks still cover changes that used the ordinary gate.

`nitely verification` reads branch health. `--project APP` also returns bounded audit
results. `nitely verification --now` explicitly runs opted-in Full checks immediately,
including retrying an infrastructure failure; known product failures continue recovery.
It never waives a check or enables disabled policy.
Overview shows branch health, the last Full pass and any recovery issue. These
timestamps describe successful verification; a later failed attempt is represented by its
status and message. A green record is bound to its exact commit, never a moving branch name.

On failure, the worker reproduces the failure and rechecks the last green baseline. It
bisects a bounded first-parent range, then verifies the candidate's parent, the candidate
alone on the baseline, and its inverse on the failed tip. A flaky result, failing baseline,
interacting changes, merge commit or conflict pauses recovery instead of guessing a culprit.

Automatic reversal additionally requires every changed path to match `reversible_paths`.
Unset, that is every path, so automatic revert is on by default; `[]` reverts nothing.
Migrations, schema changes, releases, installation, credentials and policy files are
always excluded. Unknown external effects stay paused for inspection. A confirmed regression
disables its recorded lowering rules even when it cannot safely be reverted.

The worker files a recovery issue and prepares the inverse in that issue's own worktree.
It submits and lands through the normal gate, independent review and approval rules. It never
sets Approved or writes integration directly. A human approval requirement stays in the
Inbox. Durable recovery tickets and exact inverse checks allow interrupted work to resume;
unrelated work in the recovery checkout is preserved for inspection.

After the inverse lands, another Full check must restore branch health. While recovery or an
overdue check is outstanding, the readiness API refuses reduced verification. Release checks
also refuse a known failed or unverified integration range. No audit or backstop result
authorizes release publication by itself.
