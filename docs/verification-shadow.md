# Mechanical verification in shadow mode

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
reviewer-only conditions also prevent a Basic recommendation. No mode other than shadow
version 1 can grant an exemption; reduced enforcement is not implemented here.

Prose must be explicitly allowed Markdown/plain text without code fences, indented code,
HTML or frontmatter. Agent instructions, skills, runtime manifests and protected files are
excluded regardless of the allowlist. Comment proofs compare complete parsed syntax with
token values preserved. Documentation comments, pragmas and compiler/linter directives remain
significant. Unknown languages never fall back to text stripping.

Display-copy proof initially supports native JSX element text and literal `title`,
`placeholder` and `aria-label` attributes under the approved file rule. It does not cover custom
components, arbitrary string variables, i18n files, interpolated/escaped text, URLs, routes,
keys or value attributes. Those retain Full until an appropriate parser/location proof is
implemented. A comment plus a changed executable token is never Basic. A higher declared
level remains a higher floor even when the diff itself has a Basic proof.

The post-merge backstop and human-approved activation are separate prerequisites for reducing
checks. Shadow results alone never authorize skipping review or shipping a release.

Plans also retain a separate [affected-test selection](affected-tests.md), stable audit sample
and policy-content fingerprint. A selected test map cannot lower a protected-path floor.
Confirmed audit escapes disable the matching rule in subsequent assessments. The
[post-merge worker](verification-backstop.md) and [audit worker](verification-audits.md)
provide the durable safety mechanisms; [release verification](release-verification.md)
always requires exact candidate evidence.
