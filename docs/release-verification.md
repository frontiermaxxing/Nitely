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
stamp_paths = ["Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "src-tauri/tauri.conf.json"]
# Add the actual release-only native, installer/upgrade/migration and recovery checks here.
# Commands must fail if a required remote OS check or artifact check has not passed for this
# exact stamped tree. Ordinary source-commit CI does not certify the stamped tree.
commands = []
```

An empty `commands` list adds nothing to Full. It does **not** claim the current gate covers
native behavior or every supported platform. Do not activate Nitely's release workflow until
those release-only checks are configured and the reviewer can authenticate on CI.

## CI integration proposal

The executable adapter is `scripts/release-verify-ci.mjs`. Run the copy from a trusted integration
checkout outside the candidate directory; it refuses a candidate-owned verifier/adapter/database.
It invokes the core gate/review/check phases and, on `check`, additionally queries GitHub Actions
for the latest exact-candidate runs of `ci.yml` (all three OS jobs), `app.yml` (native Windows),
and `release-candidate.yml` (`Installer and upgrade (windows-latest)`). Skipped, pending, failed,
old-attempt and source-only checks fail closed. It hashes the downloaded Actions artifact ZIP
and requires GitHub's digest for `release-candidate-<candidate SHA>` from the passing installer
attempt. Publish the archive's tested files, never a later rebuild. The REST API's
[artifact digest](https://docs.github.com/en/rest/actions/artifacts) and
[attempt-scoped job list](https://docs.github.com/en/rest/actions/workflow-jobs) bind those checks.

```sh
node "$TRUSTED/scripts/release-verify-ci.mjs" gate --verifier "$VERIFIER" \
  --worktree "$CANDIDATE" --source "$SOURCE" --author-vendor "$AUTHOR"
# In a separate step with reviewer-only authentication:
node "$TRUSTED/scripts/release-verify-ci.mjs" review --verifier "$VERIFIER" \
  --worktree "$CANDIDATE" --source "$SOURCE" --author-vendor "$AUTHOR"
# Immediately before publication, channel update or promotion:
node "$TRUSTED/scripts/release-verify-ci.mjs" check --verifier "$VERIFIER" \
  --worktree "$CANDIDATE" --source "$SOURCE" --author-vendor "$AUTHOR" \
  --repository "$GITHUB_REPOSITORY" --artifact-archive "$DOWNLOADED_ACTIONS_ZIP"
```

**Unfinished activation dependencies:** `release-candidate.yml` does not yet exist; existing
`ci.yml` and `app.yml` trigger only on main/PRs and do not run a locally stamped synthetic commit.
The candidate workflow must run on that exact SHA, exercise installer, upgrade, migration and
recovery paths, and retain the tested release archive. The existing install check inside
`release.yml` is not that independent pre-publication job. Configure candidate triggers,
provision the reviewer secret, approve release policy, and wire the adapter before **both**
publication and promotion. Until then the helper is usable and tested but the existing release
workflows are not protected by this new gate; this is not a completed rollout.

The current release workflow is deliberately not activated by this change: it has neither an
approved `[release]` policy nor provisioned release-review authentication. Enabling it without
those prerequisites would break nightly, beta and stable publication. The following is the
required integration order, including the concrete commands to wire into `release.yml`:

1. Obtain a **trusted verifier** built from a reviewed integration commit, or a previously
   published verifier binary with a pinned SHA-256. Keep it outside the candidate checkout.
   Never invoke the candidate-built `target/release/nitely.exe` for release authorization.
   Use a fresh private `NITELY_HOME` outside the checkout, retained across all three phases.
   Fetch the trusted integration ref and all tags. CI must control this ref, not a dispatch
   input supplied by the candidate. `refs/remotes/origin/main` is Nitely's integration ref.
2. Save `SOURCE=$(git rev-parse HEAD)` **before** version stamping. Stamp the five version files
   and commit those paths locally as one child of SOURCE. Do not push this synthetic commit or
   move the source tag. Build and install-test this exact candidate with the final channel and
   signing inputs. Keep generated artifacts outside the checkout, or in explicitly ignored
   output directories, so untracked source cannot hide in the verification checkout.
3. Run Full using the trusted verifier. Keep reviewer credentials **absent** from this step,
   all setup commands, package-manager hooks, builds and artifact checks:

   ```sh
   "$VERIFIER" release-verify gate --worktree "$CANDIDATE" \
     --integration-ref refs/remotes/origin/main --source "$SOURCE" --author-vendor "$AUTHOR"
   ```

4. Install the integration policy's external reviewer CLI at an approved pinned version outside
   the candidate. Supply its authentication **only** in a separate review step. For Nitely's
   current Codex reviewer, provision a dedicated `NITELY_RELEASE_OPENAI_API_KEY` secret and a
   throwaway `CODEX_HOME`; authenticate with `codex login --with-api-key` through stdin in that
   step. Remove the throwaway auth directory in an unconditional cleanup step. Do not put a key
   in arguments, logs, artifacts, the candidate environment, or repository files. A relative
   reviewer script from the candidate is not a trusted reviewer entry point.

   ```sh
   "$VERIFIER" release-verify review --worktree "$CANDIDATE" \
     --integration-ref refs/remotes/origin/main --source "$SOURCE" --author-vendor "$AUTHOR"
   ```

5. Immediately before `gh release create`, promotion/tag mutation, or updating channel manifests,
   re-fetch the integration ref, run the check below, and verify the artifact checksums produced
   by the tested build. An integration-policy move invalidates the receipt and requires a new
   gate/review. Keep SOURCE for the nightly `--target` and tagged-source provenance checks;
   comparing the workflow SHA to the synthetic candidate would incorrectly drop attestations.
   Publish the returned identity alongside artifact checksums so source and stamped candidate
   are both explicit. Do not rebuild or alter signed artifacts after verification.

   Use the adapter's `check` command above here. The bare core `release-verify check` validates
   local receipts; it does not replace the adapter's remote OS/native/artifact checks.

`AUTHOR` must reflect the release's author policy; do not label an unknown author as another
vendor merely to satisfy cross-vendor review. If policy requires cross-vendor independence and
the release has mixed or unknown authors, resolve that policy through the owner before enabling
automation.

Each platform has its own receipt. A Windows receipt is not evidence for Linux or macOS. The
release orchestrator must make all required platform/native/artifact checks part of its trusted
release commands, using the exact stamped tree and final build inputs. A command only enforces
the configured checks; it cannot invent evidence for jobs that did not run.

`promote.yml` currently creates a stable tag before dispatching `release.yml`. Its activation
must move the verification check ahead of tag creation, or defer that tag mutation until the
verified candidate's publish job. Merely adding a check after the tag is pushed is insufficient.
