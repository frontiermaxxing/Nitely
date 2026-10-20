# Mechanical verification and staged activation

The classifier records advice during submit without changing the gate, reviewer, approval,
landing or release rules. `submit` returns a `verification` plan; MCP `get_issue` returns up
to ten historical plans. The issue timeline names the tested revision, recommended depth,
escalation reason and the fact that the existing checks still apply. A plan is not a pass.

To prepare a pilot, propose a policy with the normal human-reviewed playbook workflow.
This example is configuration documentation, not an instruction to enable it:

```toml
[verification]
version = 1
mode = "shadow"
prose = ['docs/help/.*\.md']
comments = ['src/components/.*\.(rs|ts|tsx|js|jsx)']
display_copy = ['src/components/.*\.tsx']
protected = ['src/components/Payment.*']
audit_percent = 10
```

Patterns match the complete repository-relative path. Rules are read from the immutable
integration revision used for the comparison, never from the candidate's replacement.
The record includes that revision, the exact candidate commit/tree, declaration provenance
and the pinned parser/classifier version. A changed declaration updates advice even at the
same commit. Old records remain historical; consumers must compare their revision and policy.

Version 1 recognizes edits to existing ordinary files only, at most ten files and 400 changed
lines, with at most 512 KiB per source. Unsupported grammar, parse errors, missing rules,
undeclared issues, binary content, renamed/deleted/added files, symlinks and executable-bit
changes conservatively recommend Full. Core protected paths, human/blocked path rules and
reviewer-only conditions also prevent a Basic recommendation. Known executable changes have an Integration minimum. A complete single-component input map,
no more than five files and 200 changed lines, and an Inert or Contained declaration permit
Focused checks. Missing test coverage still runs the full configured gate.

Prose must be explicitly allowed Markdown/plain text without code fences, indented code,
HTML or frontmatter. Agent instructions, skills, runtime manifests and protected files are
excluded regardless of the allowlist. Comment proofs compare complete parsed syntax with
token values preserved. Documentation comments, pragmas and compiler/linter directives remain
significant. Unknown languages never fall back to text stripping.

Display-copy proof initially supports native JSX element text and literal `title`,
`placeholder` and `aria-label` attributes under the approved file rule. It does not cover custom
components, arbitrary string variables, i18n files, interpolated/escaped text, URLs, routes,
keys or value attributes. Those use the behavioral floor when their syntax is understood, and Full otherwise. A comment plus a changed executable token is never Basic. A higher declared
level remains a higher floor even when the diff itself has a Basic proof.

The post-merge backstop and human-approved activation are separate prerequisites for reducing
checks. Shadow results alone never authorize skipping review or shipping a release.

Plans also retain a separate [affected-test selection](affected-tests.md), stable audit sample
and policy-content fingerprint. A selected test map cannot lower a protected-path floor.
Confirmed audit escapes disable the matching rule in subsequent assessments. The
[post-merge worker](verification-backstop.md) and [audit worker](verification-audits.md)
provide the durable safety mechanisms; [release verification](release-verification.md)
always requires exact candidate evidence.

## Active enforcement

After the agreed shadow observation period, the human can approve `mode = "active"`
and a nonempty `basic = ["your syntax and lint command"]` in the committed playbook.
The Basic command list is project-specific; no candidate may replace its own policy.
Post-merge verification must be enabled with a recorded healthy baseline. Missing,
overdue or failing health uses the full configured gate instead of reducing checks.

Basic executes the configured checks plus `git diff --check`; it records review as
exempt, never as a completed peer review. Focused and Integration execute Basic checks
and the complete affected-test selection, retaining peer review. Cargo discovery and
ignored-test evidence must show active tests, followed by a nonempty passing test run.
Node suites must provide nonempty passing TAP evidence. A reduced gate records its
commands, depth, exact tree and authoritative policy, all rechecked before merge.
Reduced candidates land individually; combined batches retain their configured gate.

The diff starts at the merge base while policy comes from current integration. Unrelated
integration commits cannot inflate the candidate diff or let an old candidate policy
replace newly approved rules. Confirmed rule escapes disable reductions, and reduced
landings feed the durable audit queue. YOLO is an independent explicit project override;
it remains visibly unverified and supplies neither a Basic pass nor a Full baseline.
