# Project review modes

A project is either **Development** or **Tracking**. Development, the default, tests and reviews
each change before it lands, as one of the presets below sets. Tracking checks nothing: Nitely
coordinates the agents and lands each change as soon as it is handed in. It suits a project that is
not code, such as research or writing. [tracking.md](tracking.md) explains it and when to use each.

Open a project's **Settings → Review mode**. Choose Development or Tracking; under Development, pick a preset, inspect the settings it selects, and
save. Changing any individual setting switches the selection to Custom. The mode applies to the
project's connected repositories. Required human approvals and release verification stay separate.
The landing model behind the presets is in [verification-settings.md](verification-settings.md).

| Mode | Before landing | Review | Goals | Lands | Full suite on main |
| --- | --- | --- | --- | --- | --- |
| Fast | Compile, lint and the affected tests | One review per batch of changes with no goal | Through a goal branch, checkpoint every 10 items or daily | Batches of up to 5 | Every 5 lands or 30 minutes |
| Careful | Compile, lint and the affected tests | Each change with no goal; goal items without blocking | Through a goal branch, checkpoint every 5 items or daily | Batches of up to 5 | Every 5 lands or 30 minutes |
| Release | The full suite | Each change, blocking | Straight to main | Each change alone | After every land |
| Custom | Your choice, else Nitely's default | Your choice, else Nitely's default | Your choice | Your choice | Your choice |

In a repo with managed `[[submodules]]`, a goal item that moves a submodule pointer skips the goal
branch: it lands on main alone, both repos together, with the review of a change with no goal. The
issue says so. Every other goal item goes through the goal branch as above.

Every preset keeps the full suite on main, with auto-revert, and audits 10% of reduced checks
(Release reduces nothing). A protected, flagged or unmapped change runs the full gate under any
mode.

**Policy lives only in the mode.** The playbook holds facts about the repo: the gate commands, the
quick commands (`[verification] basic`), the test map, protected and human paths, the reviewers
and lanes. Its old policy keys (`[review] enabled` and `batch`, `[land] batch`, `[verification]
mode` and `audit_percent`, and `[verification.backstop] enabled`, `landings` and
`delay_seconds`) are retired: Nitely ignores them and says so on the Rules page.

**Custom** exposes each control. A control left unset uses Nitely's default, never the playbook:
the full suite before landing, a review of each change, each change landing alone, no goal
branches, 10% audits, and the full suite on main off (nothing reduced lands). Only Custom can
turn the full suite on main off or skip tests and review entirely; the strip shows either in red.
A zero periodic time limit means only the merge threshold triggers a periodic review.

Basic uses the committed integration `verification.basic` commands. Affected-test checks also
require the test map and the conservative classifier. Unsupported or protected changes, disabled
lowering rules and an unhealthy enabled backstop fall back to the full gate. Audits are zero (off)
or 10–100 percent. Confirmed escapes still disable their attributed rule.

Batch landing keeps each change's patch-bound review and human approval, and runs the full gate
on the combined tree. Changing the mode never fails work queued under a looser one: work checked at
least as strictly as the new mode asks still lands (NIT-487); anything else is submitted again.

Turning shared lanes off uses a memory-admitted machine slot in the task checkout; full backstops
use their own exact detached checkout. Turning review off leaves reviewer-only approval conditions
unanswered; those can still require a human.

**Review rounds that stall.** A change's review rounds count as stalled when a P0/P1 finding is
still open after two rounds that tried to fix it, when a new P0/P1 finding names the same `Class:`
as a P0/P1 finding of an earlier round (fixing one case at a time is not converging; `Class: none`
never matches), or after a number of rounds in a row that ask for changes: 5 in every preset,
anything from 2 to 20 under Custom (Settings, "Rounds before they stall"). What then happens is
`stalled_reviews`: the agent splits the change, or you decide each open finding
([approvals.md](approvals.md)). Neither the limit nor what happens decides how a change is checked,
so changing them never sends queued work back. The Rules strip shows both under review.

Periodic reviews are non-blocking and off in every preset (the checkpoint review covers main).
Custom can turn them on: each reviews a fixed range from its last reviewed commit to a captured main
commit, and errors keep the range for bounded retries. Findings reopen an unambiguously responsible
card or become linked follow-ups.

Projects on a retired mode moved to the closest preset (YOLO to Fast, Balanced to Careful,
Thorough to Release), and Settings shows what changed until the person saves a mode. The YOLO
switch and its table are gone. Saving a mode is a human action with revision checks, so two
windows cannot silently overwrite each other. Agents can read the selected mode and its effective
settings.

These modes control Nitely's automatic work. Explicit manual checks still run. Hosted CI follows
its own workflow configuration, and release verification always uses its own requirements.
