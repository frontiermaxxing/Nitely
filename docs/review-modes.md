# Project review modes

Project Settings can select YOLO, Fast, Balanced, Thorough or Custom. The human's
revision-checked database setting controls automatic submission and landing. It does
not rewrite repository playbooks, release checks, manual commands or human approval
rules. Existing YOLO projects retain YOLO; other projects start as Custom with every
control inherited from their repository. A settings change invalidates submission
evidence from the previous settings revision, including changing away and back.

| Mode | Automatic tests | Pre-merge review | Landing | Other automatic work |
| --- | --- | --- | --- | --- |
| YOLO | Off | Off | Batch waiting changes | Off |
| Fast | Configured Basic commands | Off | Batch waiting changes | Periodic review after 5 merges or 30 minutes |
| Balanced | Risk-based | Batched | Batch waiting changes | Full backstop after 5 light/unchecked merges or 30 minutes; 10% reduced-change audits; periodic review after 5 merges or 30 minutes |
| Thorough | Full configured gate | Individual | Individual | No redundant backstop, sampled audit or periodic review |

All presets use shared build lanes. Custom exposes each control independently; a
null override means repository settings. Periodic review has no legacy repository
setting and defaults off. A zero periodic delay disables the time threshold. The
periodic worker and its history are delivered separately from the mode authority.

Basic uses committed integration `verification.basic` commands. Risk-based checks
also require complete mappings and the existing conservative classifier. Missing
commands, unsupported or protected changes, disabled lowering rules and unhealthy
enabled backstops fall back to Full. Sampled audits apply to reduced verification;
zero disables sampling, otherwise the supported range is 10–100 percent. They do
not turn missing reviewer output into a pass. Confirmed escapes still disable their
attributed rule.

Batch landing retains each change's patch-bound review and human approval. Explicit
batching can combine Basic/risk-based submissions, but the combined tree runs a Full
gate. Legacy mechanically exempt reviews still land individually. A settings change
while waiting cancels or reroutes obsolete work and requires fresh submission evidence.

Turning shared lanes off uses a memory-admitted machine slot in the task checkout;
Full backstops use their own exact detached checkout. It does not disable machine
resource admission. Manual run/gate commands retain their explicit behavior.

Skipped automatic tests remain unverified, with `passed=false` and a reason. Turning
pre-merge review off leaves reviewer-only approval conditions unanswered; those can
still require a human. Release verification always retains its independent exact
candidate Full checks and review.
