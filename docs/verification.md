# Verification proportional to the change

Design for [NIT-297](http://127.0.0.1:7423/i/NIT-297), based on the implementation at
`01674013ea009fd25d8956689fe1a67095a8c277`. This is a proposed policy, not an enabled gate.
The current playbook and required CI checks remain authoritative until a human approves a
specific migration. The Performance delivery keeps its existing validation.

Three choices in it are decided (confirmed 2026-10-20) and implementation treats them as fixed:
a post-merge failure auto-reverts the culprit bisect isolates; Basic changes (inert, and
display copy) land with no pre-land review and are audited after landing; and a display-text
literal can be Basic. What remains for the person is the concrete policy proposal at the end
of the shadow phase (see [Rollout](#rollout-and-implementation-boundaries)).

An ordinary edit should receive fast, useful checks promptly. More expensive checks should
follow the behavior and boundaries the edit can affect. A one-line change to approval is
high risk; a long prose correction need not run every database lifecycle test. Small size
alone never proves safety. The aim is that most everyday edits use Basic or Focused checks,
not that Nitely labels most edits Basic regardless of evidence.

## One model, four visible levels

Keep **change risk** separate from **verification depth**. Existing planning names describe
risk: inert, contained, standard and sensitive. The app presents the resulting minimum test
depth as Basic, Focused, Integration or Full. Store both rather than introduce competing
review-level systems. A sensitive path can raise the minimum even for an inert-looking diff.

| Visible level | Default risk mapping | Mechanical eligibility | Required work before landing |
| --- | --- | --- | --- |
| Basic | Inert, or display copy | Only approved prose files; supported source whose executable syntax is unchanged (comment-only); or a display-text literal (below). No protected paths, generated inputs, directives or executable documentation | Diff/encoding/format checks, lint and type checks, and applicable document/config validators. No behavior suite, and no pre-land review: an audit sample after landing covers it. |
| Focused | Contained | One mapped component, at most five changed files and 200 added plus removed lines, complete dependency mapping, no public contract or protected behavior change | Basic checks, component lint/type/build checks, affected unit/component tests and a regression demonstrating changed behavior. |
| Integration | Standard | Impact is known but crosses mapped components, process/API/UI boundaries, or exceeds Focused size limits | Focused checks for all affected components plus the workflows across their changed boundaries; relevant native and platform checks. |
| Full | Sensitive or uncertain | Protected behavior, broad impact, incomplete classification or an unknown dependency boundary | Entire configured suite and required OS/native matrix, plus relevant security, migration, recovery or performance checks. |

These are starting limits for shadow evaluation, not observed safety thresholds. Basic has
an initial ceiling of ten files and 400 changed lines; larger proven-prose changes move to
Integration's applicable document checks, not an unrelated executable suite. A level selects
depth **within the affected domain**; a high label does not invent tests for absent domains.
Full uncertainty means all domains are affected until proven otherwise.

Basic lands with **no pre-land review**: a prose, comment-only or display-copy change runs its
cheap checks and lands. A sample is audited after landing ([NIT-302](http://127.0.0.1:7423/i/NIT-302)),
and an escape tightens the rule that let it through (see Rollout, step 4). Focused, Integration
and Full keep the project's independent review. Where a level reviews, the review stays peer
strength: no weaker model gates a stronger one's work. Every level keeps the blocked-path and
human-approval rules; skipping review never waives tests or approvals, and Full does not by
itself create a new human-approval requirement. An unevaluated reviewer-only condition
remains unevaluated under the existing rules.

## Classification that agents cannot lower

At filing, the issue author or goal planner may supply a declared risk and rationale. Record
the actor, source, policy version and time. A person's edit of that declaration has the same
provenance. It expresses intent, not permission to bypass the computed minimum. A declaration
by an agent that also implements its issue is only a hint. Priority and project focus are
independent: a low-priority change can still require Full verification.

At submit, classify the entire committed diff from its integration base, including both
paths of renames, deleted files, executable-bit changes, binaries and submodules. Recompute
after a rebase, dependency change, policy update or new commit. Do not classify only the most
recent commit or accept an author's description as proof. Unknown files, parse errors and
missing base trees fail closed to Full. A dirty worktree is reported but cannot supply landing
evidence; ordinary `run` can still validate its uncommitted snapshot for development.

Compute the minimum as the maximum of the mechanical risk floor, protected-path floor,
dependency-impact floor, applicable review escalation and any explicitly requested higher
level. Human declarations and agent requests can raise this result. Lowering an enforced
floor requires a separately approved policy change, never an issue field or command flag.

Initial automatic escalation rules:

- **Full:** approval/authentication/secrets, destructive Git operations and landing, migrations
  and persisted schemas, recovery/locking/process containment, installation/update/release,
  dependency or toolchain manifests, build generators, test selection, CI or verification
  policy. Changing tests to remove assertions, ignore failures or weaken budgets also raises
  the floor until reviewed under Full. Runtime configuration is behavior, even one literal.
- **At least Integration:** shared UI primitives/tokens, public types or protocols, Tauri
  command contracts, CLI/MCP routing, cross-component state and native platform adapters.
  A shared root without a complete reverse-dependency graph is Full instead.
- Existing `human_paths` remain approval conditions and also conservatively raise verification
  to Full during the initial rollout. Review findings can add coverage but cannot downgrade
  these floors. A failing or missing mandatory check blocks landing; it is not a reason to
  select a cheaper level.

Use a human-owned component/protected-path map. A change to the map is evaluated using the
previous authoritative policy, so an agent cannot make its own patch eligible by editing the
selector alongside it. Keep test-depth escalation and approval reasons visible separately.

### Proving inert and contained edits

Path patterns are a first filter, not semantic proof. Basic prose excludes `AGENTS.md`, skills,
agent instructions, playbooks, executable examples, doctests, embedded scripts and files read
as runtime data. A Markdown extension does not make executable code fences inert. Binary,
generated, vendored, symlink and permission changes are never automatically Basic.

For supported Rust, TypeScript/TSX and JavaScript, parse both revisions with pinned tree-sitter
grammars. Compare a normalized executable syntax tree with token values preserved, omitting
only ordinary comments and whitespace. Preserve directives, pragmas, documentation tests,
source-map instructions, template/string contents and compiler/linter annotations. Parse
success plus equal normalized trees is necessary, not sufficient: an allowlist must also
exclude languages/build steps that inspect raw source or comments. Unknown grammar versions
or unsupported languages cannot use this proof. Until the parser/allowlist implementation
exists, only explicitly approved non-executable prose can qualify for Basic.

A **display-text literal is Basic** when two things hold. The parser proves that only the
literal changed, and a human-owned rule marks its location as display copy: JSX text, i18n and
message files, and UI label props (`label`, `title`, `placeholder`, `aria-label` and the like).
A parser can establish that only a literal changed; it cannot establish that the literal is
harmless, so the location rule carries that half, and it belongs to the person like the
protected-path map. Interpolation placeholders must survive unchanged.

Any other literal-only change is **Focused at minimum**. Routes, keys, identifiers, SQL,
limits, regular expressions, URLs, command text, and messages in protected paths are never
display copy, whatever file they sit in. They reuse the ordinary component checks and
appropriate render/accessibility tests. New behavior needs a regression test, but avoid tests
that merely repeat the implementation or assert a cosmetic spelling with no behavior at stake.

## Selecting checks

Basic selects no behavior tests, so phase 1 needs none of what follows; it is the design for
Focused and Integration, built after it. Build a versioned test inventory: stable test ID, component, source inputs, reverse consumers,
fixtures, environment/platform requirements, estimated cost, and suite membership. Record
where each dependency came from (build graph, static import, approved mapping or measured
coverage). Runtime coverage may add tests; absence from a coverage run cannot prove a test
irrelevant. Dynamic imports, generated code, macros and external tools need explicit edges.

Selection includes changed tests, tests for changed symbols/components, reverse dependents,
shared fixtures and boundary contracts. A deleted file still has consumers. A changed test
helper selects every suite using it. Include the lint/type/build context needed to validate
that selection; a Rust test filter alone does not type-check every affected crate or target.

An unknown edge within a component selects that component's whole suite. An unknown edge
across components, missing inventory, changed generator, unresolved platform feature or stale
mapping selects Full. Zero selected tests is not a pass for changed behavior: require an
explicit non-behavior proof or escalate. Match test IDs, discover them before execution and
fail if required IDs are absent; a successful runner exit with zero matched tests proves
nothing. Report ignored and quarantined tests separately.

For Nitely, the first practical split is frontend versus core/CLI/native boundaries:

- Frontend edits can cheaply retain `npm test` and `npm run build` while the component selector
  matures. The current test runner compiles all tests and executes frontend and script tests;
  it does not yet accept a reliable affected-test manifest. Do not pretend a filename filter
  already provides this feature.
- Pure mapped core helpers can use scoped Cargo unit tests plus affected crate static checks.
  Database, Git, land, crew lifecycle and native command changes add their actual integration
  suites; approval, recovery and other protected changes remain Full. Do not route every core
  edit to all Git-repository fixtures merely because they share one crate.
- Native window/notification behavior requires the applicable Windows harness in CI on
  throwaway data. Browser preview is useful for rendering, but is not native evidence. Changes
  to harness infrastructure require its wider scenarios. OS adapters require their supported
  platform jobs; unavailable CI is pending, never a pass.

Run cheap checks first and publish their results promptly. Review may run alongside deeper
validation on the same immutable revision after cheap checks pass; landing waits for both.
This overlap is a proposed executor change, not current `submit` behavior. Cancel obsolete
queued work when safe; do not duplicate a still-running gate because an MCP wait expired.
Use build lanes and existing machine admission throughout. Process-wide performance budgets
run serially; keep fixture-heavy test concurrency separate from compiler concurrency when
measurement supports doing so.

## Evidence and landing

Persist a verification plan and per-check results, not just a level badge. Bind each to:

- Repository identity, full base and candidate commit IDs, candidate tree, changed-patch ID
  and integration candidate/stack membership.
- Policy, classifier, parser and dependency-map versions; classification facts and escalation
  reasons; resolved ordered commands/test IDs and required platform matrix.
- Relevant toolchain/dependency lock hashes, build features/target, environment identity and
  input fingerprints. Do not store secret environment values.
- Start/end times, queue/setup/execution durations, exit state, attempts, ignored tests,
  log/artifact references, and explicit executed/reused/skipped/deferred/pending statuses.

Only a passing result with compatible inputs satisfies a required check. Timeout, resource
failure and test assertion failure remain distinct. Preserve the first failure on a successful
retry and file/associate the flake; never hide it behind a green level. Initially preserve
current retry policy. A later test-only retry optimization requires an approved isolation rule
and proof that omitted checks already passed at the exact same inputs.

Current lane reuse is repository + complete HEAD tree + ordered commands, restricted to
successful local runs that finished without a timeout or error. A `gate`/`land` run qualifies, and
so does an ordinary `run` whose single command is the gate's exact commands in order joined by
`&&`, so each ran only after the one before passed ([NIT-455](http://127.0.0.1:7423/i/NIT-455)).
A run of uncommitted work has a snapshot tree of its own and never matches; commands joined by a
line break or `;` run on past a failure and never match. The issue records which run answered the
gate. Preserve that safe baseline while introducing richer keys. Reuse on an unchanged tree is different from reusing
one component after the tree changes: the latter requires a complete, versioned input closure,
including reverse contracts, fixtures, generated inputs and environment. Otherwise rerun.
Do not infer unchanged behavior from an unchanged patch ID alone.

At land, recompute the plan against the exact rebased/stacked candidate and current authoritative
policy. Run required integration checks on that final tree. Unrelated component evidence may
carry only under the closure rule above; a changed ancestor invalidates any dependent evidence.
Retain existing exact-patch review and signed human-approval checks, and recheck policy before
the ref update. A batch uses the union of component requirements and the strongest applicable
floor, with stack-specific boundary checks. Approval stays per issue; batching cannot absorb a
human approval. The existing land queue's pre-merge bisection is not post-merge recovery.

On the issue, PR and board show the effective level, why it was selected, completed/remaining
checks and the tested revision. Expand to concrete commands, platforms, reused evidence and
skipped/deferred coverage with its next required checkpoint. A Basic pass never says “full
suite passed.” Keep solution metadata bound to the revision under the existing submit flow.

## Integrated and release backstops

Before enabling reduced pre-merge checks, implement a durable post-merge verification queue.
Proposed starting cadence: queue Full verification at five reduced-verification landings or
30 minutes after the first unverified landing, whichever comes first; run a nightly sweep
when the integration branch changed. Coalesce pending heads, not evidence requirements, and
retain the exact verified tree and the range it covers. Use CI where available rather than
starting an uncontrolled extra local build. If the backstop is overdue and cannot run, pause
further reduced-verification landings; the machine's resource admission still takes precedence.

A post-merge failure blocks release/promotion until it is resolved. Record the failure, last
verified base and candidate range; triage flakiness/infrastructure separately (a failure that
does not reproduce on the failing tree is a flake to file, not a culprit to revert). Bisect in
isolated worktrees using the failing command first.

- **Bisect isolates one culprit: revert it automatically.** This is the default. The revert
  lands as a normal change, with the integrated checks that confirm the failure is gone, on
  top of current main. Nitely files an issue for the failure, and reopens the culprit's issue
  with the failing log, so its author fixes it and lands it again.
- **Bisect cannot isolate one** (interacting changes, where no single commit fails alone):
  pause reduced-verification landings and file an issue with the range and the log. Do not
  blame the final commit of a failing prefix.
- **Never revert a migration automatically.** A culprit that carries a migration, or other
  effects outside the repository (a published artifact, an external call), pauses and files
  instead: undoing it can lose data that the revert's code no longer reads.

Reduced-verification landings resume once main passes the failing check again.

Releases require Full results on the exact release candidate, all required OS/native jobs,
relevant install/upgrade/migration/recovery checks and the configured release review/approval.
Packaging changes invalidate relevant evidence. Use the tested artifacts or verified matching
build inputs for publication; a nightly green on a different tree does not authorize release.
No unsupported platform or skipped benchmark becomes a fictional pass.

## Rollout and implementation boundaries

Phase 1 ships value in days, with Basic alone; it needs no test inventory, because Basic runs
no behavior suite and every other level keeps today's gate until affected-test selection exists.

1. **Phase 1: declarations, Basic and the backstop.**
   [Declarations](http://127.0.0.1:7423/i/NIT-298) on the issue; a minimal
   [mechanical check](http://127.0.0.1:7423/i/NIT-299) for prose-only, comment-only and
   display-copy changes; and the [post-merge backstop](http://127.0.0.1:7423/i/NIT-301) with
   auto-revert. Store old issues as undeclared, not implicitly Basic. Unsupported/missing policy
   means legacy Full behavior.
2. **Shadow, briefly.** Compute and show each change's level and plan while every current
   gate/CI check keeps running, and compare. Shadow lasts **one week or 30 Basic candidates,
   whichever comes first**. Every Basic candidate in that window is inspected; a wrong-low
   classification fixes its rule before the proposal.
3. **One proposal.** The result goes to the person as one concrete playbook proposal: the
   display-copy locations, the prose and comment rules, the backstop cadence and the audit
   sample. On approval, Basic lands with no pre-land review. A test-depth change alone does not
   override a person's project-wide review setting for the other levels.
4. **Later: Focused and beyond.** The versioned test inventory,
   [affected-test selection](http://127.0.0.1:7423/i/NIT-300) for Focused components,
   [audit sampling](http://127.0.0.1:7423/i/NIT-302) after landing (a stable sample selected
   once per issue/policy using a server-owned salted hash, persisting across revisions, so a
   changed declaration cannot reroll it; at least 10% of reduced candidates once Focused is on)
   and the [release gate](http://127.0.0.1:7423/i/NIT-303). Each new lowering rule goes through
   the same shadow and proposal before it activates.
5. **Broaden from evidence.** Track wrong-low classifications, escapes, missing test mappings,
   fallback reasons, declaration accuracy by rule/source, queue/run durations, retries,
   CPU/memory/I/O and total developer latency.
   A confirmed escape disables the affected lowering rule and returns that scope to Full until
   a regression and revised rule pass shadow evaluation. Publish uncertainty, not a “safe” rate
   inferred only from passing runs.

The current implementation supplies extension points, not this feature:

| Existing code | Extension |
| --- | --- |
| `playbook.rs`: `GateRules`, `GateStep`, `run_gate_as` | Versioned verification policy and resolver; reuse conditional-command syntax without treating paths as semantic proof. |
| `issues.rs`, `goal_plans.rs`, CLI/MCP filing and human issue controls | Declared risk with provenance; agent declarations cannot authorize downgrades. |
| `submit.rs`: gate then review, solution/patch records | Persist the plan and evidence; later overlap review with validation on an immutable revision. |
| `lanes.rs`: `gate`, `lane_run`, resource admission | Execute check manifests, preserve retry/resource accounting and extend cache keys. |
| `land.rs`, `land_queue.rs` | Final-tree plan resolution, exact evidence checks and union of batched requirements. |
| `scripts/test.mjs`, Cargo test inventory, harness scripts | Explicit suite/test manifests and dependency coverage; fail on empty selection. |
| `.github/workflows/ci.yml`, `app.yml` | Current three-OS gate and Windows native checks become policy-aware only through reviewed CI changes. |

The integration branch's authoritative policy controls classification, never the candidate's
unapproved replacement. A policy proposal must show exact changed rules, fallbacks, audit and
backstop readiness; the human approves it. Existing approval signatures and blocked paths stay
intact. Update local gates, CI required checks and release enforcement together, including the
test that checks gate/CI agreement. Old clients cannot interpret a new policy as an empty gate:
reject unsupported versions or use its explicit legacy Full command set. Keep rollback to Full
available without losing prior evidence. Do not edit the live playbook as part of this design.

Implementation remains in the existing issue chain:
[declarations](http://127.0.0.1:7423/i/NIT-298),
[mechanical verification](http://127.0.0.1:7423/i/NIT-299),
[affected tests](http://127.0.0.1:7423/i/NIT-300),
[post-merge backstop](http://127.0.0.1:7423/i/NIT-301),
[audit](http://127.0.0.1:7423/i/NIT-302) and
[release gate](http://127.0.0.1:7423/i/NIT-303).

## Measurements and their limits

Read-only inspection on 2026-10-19 used the latest 500 first-parent commits ending at
`01674013ea009fd25d8956689fe1a67095a8c277` (commit dates 2026-10-16 through 2026-10-19).
Command: `git log --first-parent -500 --no-renames --format='@@%H%x09%cs%x09%s' --numstat`.
Count all added and removed lines and both sides represented in path changes. No builds ran.

| Screening result | Commits | Share of 500 |
| --- | ---: | ---: |
| Nonempty commits inspected | 500 | 100% |
| Prose-path-only candidates | 10 | 2.0% |
| Other small candidates outside a coarse protected-path screen | 251 | 50.2% |
| Commits touching that protected-path screen | 189 | 37.8% |
| Commits containing binary changes | 2 | 0.4% |

These are overlapping screening categories, not assigned levels. Prose paths were `docs/*.md`
(including descendants), `README.md`, `LICENSE` and `CHANGELOG.md`. The small screen required
at most five paths, 200 added/removed lines and no binary change, excluding prose-only rows.
The coarse protected screen matched `.github/`, `.nitely/`, core migrations, Tauri capabilities,
root Cargo/package/toolchain manifests, and paths containing approval, keychain, credentials,
auth, land.rs, land_queue.rs, playbook.rs, review.rs, db.rs, procs.rs or install.rs. This screen
is deliberately documented so it can be reproduced; it is not the production classifier.

The sample contains only 142 distinct issue keys in commit subjects. Commits are not 500
independent landed issues; one change may contain multiple commits, and subjects omit some
keys. No AST, dependency, executable-document or complete protected-path analysis was applied.
Consequently **neither 2.0% nor 50.2% is a proven eligibility or savings rate**. Measure full
landed diffs from recorded base/tip pairs in shadow mode before making that claim. This design
does not inspect or publish other projects' history.

Existing NIT lane logs provide narrower cost evidence:

- Run 1259: initial Cargo compilation 25.51 seconds; the core suite 447.33 seconds.
- Run 1264: initial compilation 7.58 seconds; core suite 545.70 seconds; Clippy 2.89 seconds;
  frontend test-runner execution about 0.528 seconds (excluding its TypeScript compilation).
- Run 1262: core suite 542.98 seconds followed by a failed-fixture retry of 543.76 seconds.
  The recorder fixture investigation is [NIT-397](http://127.0.0.1:7423/i/NIT-397).

These are local runs, not representative percentiles. Gate duration also includes queue,
setup, other crates, builds and retries; test output durations are not CPU time. No per-test
profile or attribution proves which Git/filesystem/locking operation dominates. The evidence
supports trying affected-suite selection and profiling fixtures, not weakening coverage based
on an invented percentage saved. Gather per-check timing in the next ordinary gate rather
than launching an extra full suite solely to estimate cost.

## Acceptance corpus before a pilot

Exercise classification on real base/candidate pairs and synthetic adversarial diffs. Expected
cases include ordinary comments versus doctests/directives; prose versus an executable code
fence; a display literal versus a route/SQL/limit literal; a comment plus one changed code
token; renamed identifiers; deleted/renamed files; a small approval edit; changed test helpers;
generated inputs; unknown grammars; empty test selection; and a stale dependency graph. Check
the exact escalation reason, selected manifest and independent approval result, not only a
level label. The corpus must include genuinely safe cases to detect unnecessary escalation.

Executor tests must cover an ancestor change after a passing component check, equivalent-tree
rebases, policy changes while a gate waits, incompatible toolchains, CI pending/failure,
interrupted retries, a failing post-merge batch with interacting changes, audit persistence
across revisions, old clients and rollback to Full. A proposed lowering rule cannot activate
while its false-low corpus cases or required backstop behavior remain untested.

## Decisions before activation

Decided (2026-10-20): auto-revert of an isolated culprit is the default response to a
post-merge failure (never for a migration); Basic lands with no pre-land review and is audited
after landing; a display-text literal can be Basic.

The recommended defaults above make implementation reviewable, but do not approve activation.
At the end of the shadow phase the person approves, as one concrete policy proposal, the
display-copy locations and prose/comment rules, the post-merge cadence and overdue-queue
behavior, and the audit fraction. No human response is needed to build phase 1 and run it in
shadow.
