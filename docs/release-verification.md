# Exact release verification

`nitely release-verify gate|review|check` verifies a committed release candidate. It does not
publish anything. The command reads `.nitely/playbook.toml` from the trusted integration ref,
not the candidate's file. There is no lighter-level exemption and `review.enabled = false`
does not turn off release review.

The gate executes every `[gate].commands` entry, every conditional gate step regardless of
changed paths, and every `[release].commands` entry. It uses the machine admission controls and
fresh build-lane execution. Gate, review and final check require the same source commit,
candidate commit/tree, integration policy commit, baseline, author vendor, OS and command list.
A new failed or interrupted attempt supersedes an earlier pass. A new gate attempt requires a
new release review. Known paused, failed or unverified integration-backstop state blocks all
three phases; lack of a local backstop record does not replace the required Full gate.

The reviewer receives the whole release diff, including stamped files, from the most recent
matching release tag reachable from the source commit's parent. With no matching tag it reviews
from the empty tree. Fetch all history **and all release tags** before verification. Missing tags
can broaden the review to the first release; the tool never treats missing history as a clean
review. Use a tag pattern appropriate to the release channel. If a stable release must include
all changes since the last stable, its integration-owned policy must select stable tags rather
than beta tags. The CLI cannot choose a narrower baseline.

Human approval rules and reviewer flags fail closed. This command cannot manufacture a signed
approval, accept findings, or reuse a task approval for a whole release. Projects needing human
release approval need that approval integration before enabling unattended publication.

## Proposed policy (not active)

Have the owner approve a playbook proposal before adding this section on integration:

```toml
[release]
tag = "v*"
exclude_tags = ["*-*"] # Review every beta/nightly/stable change since the previous stable tag.
stamp_paths = ["Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "src-tauri/tauri.conf.json"]
# The trusted CI adapter independently requires all OS/native/installer checks before publishing.
commands = ["node scripts/version.mjs"]
```

An empty `commands` list adds nothing to Full. It does **not** claim the current gate covers
native behavior or every supported platform. Do not activate Nitely's release workflow until
those release-only checks are configured and the reviewer can authenticate on CI.

## Opt-in CI integration

The checked-in `verified-release.yml` is inactive unless the repository variable
`NITELY_VERIFIED_RELEASES` equals `true`. With it enabled, `release.yml` delegates nightly/tag
builds to that workflow **on main**, and `promote.yml` delegates before creating a stable tag.
Without it, existing release behavior remains unchanged. No policy, variable, secret or release
was changed as part of this implementation.

The pipeline separates these steps onto fresh Windows runners:

1. Build the verifier from the trusted integration commit. Preserve the original source SHA,
   stamp the five allowed version files, and commit one child of that source. Push the candidate
   to the unique temporary branch `nitely-release-candidate/<run>-<attempt>`. This remote ref is
   necessary so dispatched jobs and their artifacts identify the **stamped candidate SHA**;
   source tags remain unchanged. Retain source, candidate, policy, channel and version metadata.
2. Dispatch `ci.yml` (Windows/macOS/Linux Full gates), `app.yml` (native Windows harness), and
   `release-candidate.yml` at that candidate ref, with its final channel. Separately execute
   `release-verify gate` using the trusted binary and integration policy. The gate runs fresh,
   with no reduced-plan, PR-review-off or YOLO exemption and no reviewer credential.
3. On a new runner, install the approved pinned external Codex reviewer before supplying its
   credential. Download only the gate database receipt, authenticate through stdin into a
   throwaway `CODEX_HOME`, and run whole-release peer review. Always remove authentication;
   only the Nitely receipt database is retained. Candidate build/setup commands never run on
   this runner or inherit the reviewer key.
4. On another runner, wait for the latest exact-candidate CI attempts, download the raw installer
   Actions artifact ZIP, and verify GitHub's artifact SHA-256 against those bytes. The trusted
   adapter also checks each required job and revalidates the local gate/review receipt. Safely
   extract a flat, bounded archive; validate its file inventory, checksums and candidate metadata.
   Publish these tested files without rebuilding or executing candidate binaries. Attest their
   bytes and identity manifest; this attestation names the trusted publisher workflow, while
   `candidate.json` and `verification.json` record the source and stamped candidate explicitly.
5. Re-fetch integration and recheck authorization immediately before tag creation, release
   publication and each channel update. Integration moving invalidates this attempt. Stable
   promotion creates its tag only here, after review and all checks. Verified publications share
   one concurrency group to serialize channel updates. Clean up only this run's temporary ref.

`scripts/release-verify-ci.mjs` is the fail-closed adapter. It accepts no caller-supplied success
JSON or check-name list. Its fixed requirements are the three `Gate (<OS>)` jobs in `ci.yml`,
`Inbox in the real app (windows-latest)` in `app.yml`, and
`Installer and upgrade (windows-latest)` in `release-candidate.yml`. Pending/failed newer attempts,
skipped jobs, source-only checks, wrong artifacts and expired artifacts all refuse publication.
The [artifact digest](https://docs.github.com/en/rest/actions/artifacts) and
[attempt-scoped job list](https://docs.github.com/en/rest/actions/workflow-jobs) bind the evidence.

The installer job builds the stamped CLI and NSIS installer with final channel/signing inputs.
It downloads a previous published stable/beta installer, checks its published SHA-256, installs
it, writes a marker into throwaway data, installs the candidate over it, and checks migration
preserves the marker and SQLite integrity. It then injects an unknown future migration, checks
that the candidate refuses it, restores the saved database and verifies it opens with the old
marker intact. This is an upgrade/migration/recovery **smoke test**, not proof of all interruption
or rollback scenarios. It requires an existing published baseline and fails closed without one.
No native installer or app harness is run on the developer's PC.

## Activation dependencies and rollout limits

This is implementation ready for review, **not an activated or end-to-end validated rollout**.
No real release was dispatched to exercise it. The owner must resolve these prerequisites:

- Approve `[release]` policy on integration, including baseline tag selection, the five stamp
  paths, release-only commands as needed, and reviewer/approval policy. Human approval rules
  cannot be auto-satisfied. Stable baseline selection must cover the full intended release diff.
- Provision a dedicated `NITELY_RELEASE_OPENAI_API_KEY` secret. Set
  `NITELY_RELEASE_CODEX_VERSION` to an approved exact npm version and
  `NITELY_RELEASE_AUTHOR_VENDOR` to the truthful author policy (`claude`, `codex`, `cursor` or
  `human`). Mixed/unknown authors and cross-vendor requirements need an owner decision; do not
  invent an author merely to obtain an automatic pass. The integrated reviewer must be the
  trusted external Codex CLI, not a candidate-relative wrapper.
- Ensure the source being released contains the new dispatchable CI/native/installer workflows.
  Preparation requires their `nitely-exact-release-v1` contract markers and dispatch triggers
  before pushing a candidate. Old tags (including older betas) predating this integration fail
  with an explicit unsupported-source error; this implementation does not retrofit their trees.
  More importantly,
  GitHub runs a tag's historical copy of `release.yml`: an old copy lacks the opt-in switch and
  can still use legacy publication. Before claiming global enforcement, retire old in-flight
  publishers and restrict historical-tag workflow dispatch/rebuilds through repository policy
  or a separate approved migration. Setting the variable alone cannot rewrite old Git trees.
- Review a draft/test-channel CI run with provisioned policy/auth, including the actual external
  reviewer, signed/unsigned installer behavior, artifact transfer and promotion refusal paths.
  Do not mark rollout complete on local helper tests alone. First-release bootstrap without a
  prior installer, broad native failure recovery and human release approval remain explicit
  product/policy decisions outside this initial automation.

Only after those steps should the owner set `NITELY_VERIFIED_RELEASES=true`. The workflow refuses
missing prerequisites rather than silently falling back to legacy publication once opted in.
If a run fails after a tag/release mutation, inspect the retained identity and receipts; reruns do
not overwrite an existing stable release or silently move its source tag. A policy/head move
requires a new gate/review attempt. An integration update during a long run may therefore require
retrying when integration is stable.
