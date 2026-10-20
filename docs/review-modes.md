# Project review modes

Open a project's **Settings → Review mode**. Pick a preset, inspect the settings it
selects, and save. Changing any individual setting switches the selection to Custom.
Mode changes apply to the project's connected repositories; required human approvals
and release verification remain separate.

| Mode | Automatic checks before merging | PR review | Periodic review of main |
| --- | --- | --- | --- |
| YOLO | Off | Off | Off |
| Fast | Configured Basic checks, with Full fallback | Off | Every five merges or 30 minutes |
| Balanced | Risk-based checks, with Full fallback | Batched | Every five merges or 30 minutes |
| Thorough | Full suite | Each change | Off |
| Custom | Your choice or repository settings | Your choice or repository settings | Your choice |

All presets use shared build lanes. YOLO and Balanced batch waiting merges;
Fast and Thorough land each change separately. Fast avoids the Full combined gate
that batching would require for its Basic changes. Balanced additionally enables Full checks on
main after five light or unchecked landings or 30 minutes, and audits 10% of eligible
light changes. The other presets turn those two mechanisms off.

Custom exposes these controls individually, including the periodic merge threshold,
time limit, reviewer and finding behavior. “Use repository settings” preserves each
repository's configured value. A zero periodic time limit means only the merge
threshold triggers a review. Basic and risk-based checks cannot manufacture missing
commands or coverage: incomplete configuration falls back to Full verification.
Machine resource admission still applies to work that runs.

Basic uses committed integration `verification.basic` commands. Risk-based checks
also require complete mappings and the conservative classifier. Unsupported or
protected changes, disabled lowering rules and unhealthy enabled backstops fall back
to Full. Sampled audits apply to reduced verification; zero disables sampling,
otherwise the supported range is 10–100 percent. Confirmed escapes still disable
their attributed rule.

Batch landing retains each change's patch-bound review and human approval. Batched
Basic or risk-based submissions run a Full gate on the combined tree. Legacy
mechanically exempt reviews still land individually. Changing settings invalidates
evidence from the previous revision, even when changing away and back, and obsolete
queued work must be rerouted or submitted again.

Turning shared lanes off uses a memory-admitted machine slot in the task checkout;
Full backstops use their own exact detached checkout. Skipped automatic tests remain
unverified. Turning PR review off leaves reviewer-only approval conditions unanswered;
those can still require a human.

Periodic reviews are non-blocking. Each reviews a fixed range from its last
successfully reviewed commit to a captured main commit. Merges arriving during a
review remain for a later range. Enabling periodic review establishes a starting
baseline; it does not claim that older code has been reviewed. Errors remain visible,
retain the unreviewed range and receive bounded retries.

Choose whether findings should reopen an unambiguously responsible completed card
or always create linked follow-up cards. Ambiguous findings become follow-ups;
recorded findings are deduplicated. The activity section shows reviewed ranges,
reviewer failures and resulting issue links. It never labels an unavailable review
as a pass.

Existing enabled YOLO settings migrate to the YOLO preset. Other projects retain
their existing configuration through Custom with repository defaults. Saving modes
is a human action with revision checks so two windows cannot silently overwrite one
another. Agents can read the selected mode and its effective settings.

These modes control Nitely's automatic work. Explicit manual checks still run.
Hosted CI follows its own workflow configuration, and release verification always
uses its separate release requirements.
