# Set up a project in Nitely: the first-run sweep

A repo just connected to Nitely has an empty board, but the project is not empty: unfinished work
sits in TODOs, plan documents, half-merged branches, uncommitted changes, agent memory files, stub
CI and disabled config. This sweep reads the repo and turns that into issues, a project brief and
proposed rules, so `next_task` hands out the right work. Follow the phases in order.

**Rules for the whole sweep.** Read only: no commits, branches, `start_task`, pushes or deploys,
and nothing run against live systems (staging or production), ever. Never write a secret value
anywhere (issues, notes, the brief, chat): say "an AWS key is committed in config/aws.py:20", never
the key. Anything that would write to a third-party system needs the user's go-ahead first. Every
issue carries evidence: a file and line, a command and what it printed, or "inferred" with why.

## Before you start

1. `get_project`, and read the brief. If `setup` is set, this project was swept before (at
   `setup.sha`): this is a refresh. Say so, and with the user's go-ahead follow **Refresh** at the
   end instead of the phases (asked for a refresh, as with `/nitely-setup --refresh`, go straight
   there).
2. Tell the user what the sweep costs and ask to go ahead. Scale from: a repo of about 200k lines
   took about 5 parallel read-only agents, 20 to 30 minutes and roughly 800k tokens, and filed
   about 90 issues. A small repo (a few thousand lines) takes one pass and a few minutes.

## Phase 0: pre-flight (cheap, always first)

- **Integration branch.** Compare the playbook's integration branch with the other long-lived
  branches: `git for-each-ref --sort=-committerdate refs/heads refs/remotes`, ahead/behind counts
  (`git rev-list --left-right --count A...B`), last commit dates and `origin/HEAD`. If it is stale
  (months behind another branch, or an ancestor of a much newer one), stop and do this first:
  file it as an issue, `propose_playbook_change` the right branch with the evidence, and `ask` the
  user before anything else. It is the first question of the sweep; everything else depends on it.
- **Gate commands.** Find what really verifies a change: CI workflows, package scripts, Makefile
  targets, test folders. If the playbook's gate is missing or wrong, `propose_playbook_change` the
  commands, with where you found each. Never change the playbook yourself.

  Put the branch and the gate in one proposal when you can. A later proposal for the same repo
  replaces the pending one with a new issue that carries every change (Nitely closes the old one
  as its duplicate), so word your questions about the problem, not about a proposal's key.
- **Working tree.** `git status`. Uncommitted work is someone's unfinished change: file it as one
  issue (what it touches, `git diff --stat`), never commit or discard it.
- **What the project says about itself.** Read README, AGENTS.md / CLAUDE.md, docs/, plan
  documents and their checklists, and the agents' memory folders (for Claude Code,
  `~/.claude/projects/<the repo's path, with separators as dashes>/memory/`). Memory is often the
  richest source of known problems and decisions. Note every item it names as known work.

## Phase 1: areas

Split the repo into 3 to 6 sweep areas from its layout (for example "api", "web app", "mobile
app", "infra and CI"), plus two that are always there: **docs, process and hygiene**, and
**product map** (what the product does for its users, screen by screen or endpoint by endpoint,
and what is missing or half built). For each area, list the known items from Phase 0 that belong
to it.

## Phase 2: sweep each area

If you can run subagents, start one read-only explorer per area, all at once. If you cannot (some
agents have none), sweep the areas one after another yourself. Each sweep gets:

- a short summary of the product, and the area's paths;
- the known items for the area, each marked "verify it is still true";
- the hunt list: TODO/FIXME/HACK, public endpoints that write data (and whether they check who
  calls), swallowed errors, committed junk or secrets, unpinned or outdated dependencies, files
  over 2,000 lines, dead duplicates, disabled or commented-out config, tests that never run (not
  in CI, skipped, or broken), stubs and half-built features;
- the rules above (read only, never print a secret, file:line evidence);
- this output format, one JSON object per finding:

```json
{ "title": "Signup endpoint accepts any role from the client", "kind": "bug", "priority": 1,
  "body": "What is wrong or missing, why it matters, and what done looks like.",
  "evidence": "api/users/views.py:88 copies request.data['role'] into the new user",
  "verified": "read-code", "human_only": false, "depends_on_titles": [] }
```

`kind` is bug, feature, improvement, idea or live-check. `verified` is read-code (the sweep read
the code that shows it), inferred (from docs, names or structure) or needs-live-test (only running
it would tell). `human_only` is true for work no agent can do: buying, signing, a console only the
owner can reach, a decision about the product.

## Phase 3: check the important ones

For every priority-1 finding, read the cited code yourself before filing it. If it holds, keep
`read-code`. If you cannot confirm it, file it anyway with "Not yet verified: " at the start of the
body and say what would confirm it. Never confirm anything by running it against a live system.

## Phase 4: consolidate

- Merge the same problem found in several areas into one issue with all the evidence.
- Merge small items that touch the same code and would be done together.
- Split an item whose parts differ in severity, or in who must act (an agent's part and the
  owner's part).
- Drop what the code shows is already done.
- Check the board for what is already filed (`list_issues` with `include_closed`), and drop or
  link those.
- Order by priority, then by dependency.

Priorities: **1** broken for users, data loss, a security hole or a leaked secret; **2** wrong
behavior, or work that blocks other work; **3** worth doing (cleanup, missing tests, docs);
**4** nice to have. Anything speculative goes to Backlog, not Todo.

## Phase 5: file

File everything with one `file_issues` call (in batches if very large). Give each item a `ref`, and
turn `depends_on_titles` into `blocked_by` refs. Give each the area (and platform, if the repo has
several) as `labels`, e.g. `["api"]` or `["mobile", "ios"]`: one short lowercase word per area, the
same across the batch, reusing labels the project already has. Human-only items get `owner: "human"`
(they go to the owner's Needs You, and `next_task` never hands them out). Ideas and speculative
items go in `state: "Backlog"`.

`file_issues` skips an item that looks like an open issue or an earlier item of the batch, and
lists it under `skipped` with the issue it stands for: read that list. When you split one problem
by who must act (an agent removes a leaked key from the code, the owner rotates it), the two look
alike, so set `allow_similar: true` on the second.

If there are more than about 60 issues, show the counts by priority and by area first, and ask
whether to file all of them or only priorities 1 to 3.

## Phase 6: rules, brief, and wrap-up

- **Review and approval rules.** Draft the playbook's judgment for this project and propose it
  with `propose_playbook_change`. Open `why` with one short sentence that sums the change up ("Review
  rules for the billing and release code"): it becomes the title of the person's decision. Then
  explain each rule:
  - `review.focus`: what matters most in this codebase, in a few sentences;
  - `approval.human_paths`: paths whose changes need a person, each with a reason (money and
    billing, release and deploy, auth, data migrations, secrets handling);
  - `review.flags`: yes/no questions for risks path rules cannot see;
  - `approval.blocked_paths`: paths that change some other way (generated, vendored, a submodule);
  - `approval.always_human = false`, when the playbook has `always_human = true`: Nitely starts a
    new repo that way so nothing lands unseen until rules exist, and the approved rules replace it.

  For example (made-up paths):

  ```toml
  [review]
  focus = "Payments and the public API matter most: a change that can charge twice, lose an order or break an API client is a P1."

  [[review.flags]]
  name = "MONEY"
  question = "Does this change how much a customer pays, when, or what is recorded about it?"
  reason = "touches money"

  [[approval.human_paths]]
  pattern = "^(billing|payments)/"
  reason = "billing and money"

  [[approval.human_paths]]
  pattern = "^(deploy|\\.github/workflows)/"
  reason = "release and deploy"

  [approval]
  always_human = false

  [[approval.blocked_paths]]
  pattern = "^vendor/"
  reason = "vendored code is updated by the vendoring script"
  ```

- **First goals.** Group the work into the 1 to 3 outcomes the target order leads to, and draft
  each with `propose_goal`: a title, a one-paragraph outcome, and the issues you filed in Phase 5
  that belong in it as `existing` (add new issues only for gaps). The person approves each in the
  Inbox; the board then shows a row per goal with how far along it is.
- **The brief.** `get_project`, then `set_project_brief` with the revision it returned:
  - **Vision**, marked as inference ("Inferred from the code, not confirmed by the owner");
  - **State of each area**: what works, what is half built, what is missing, with issue links;
  - **Target order**: what to do first and why;
  - **Questions for the owner**: 3 to 5 that only they can answer, also asked with `ask`.
- **Mark it done.** `mark_setup_complete` with the commit you swept (`git rev-parse` of the
  integration branch).
- **Close the setup issue.** When a repo is connected, Nitely files "Set up …: sweep the repo, seed
  the board and draft its review rules". If you took that issue, move it to Done with what you filed
  as evidence.
- **Tell the user**, in chat: how many issues by priority and area, the priority-1 list, what is
  in Needs You, what was not verified, and the Phase 0 question if it is still open.
- **Leave a handoff**: `note` with `handoff: true` on the first issue to do (or on the stale-branch issue):
  what the sweep covered, what it did not, and what was not verified.

## Refresh: sweep a project that was set up before

A board drifts after its first sweep: new TODOs and branches, plans that moved on, issues fixed by
the way. A refresh files only what is new, and run straight after a sweep it files nothing.

1. **What changed.** `get_project` gives the commit the last sweep read (`setup.sha`). Read
   `git log --oneline <sha>..<integration branch>` and `git diff --stat <sha>..<integration branch>`,
   plus branches created since, the working tree, and the agents' memory files changed since. That
   is where most new work is.
2. **Pre-flight again** (Phase 0): the integration branch, the gate and the working tree. Propose
   only what is wrong now.
3. **Sweep the changed areas fully** (Phases 1 to 3, restricted to the areas the diff touches), and
   the rest lightly: only the hunt list's quickest checks.
4. **Check every candidate against the board**: `list_issues` with `include_closed: true`. Work
   already filed, finished or canceled is never filed again; `file_issue` and `file_issues` also
   refuse what looks like an existing issue, so read `skipped`.
   - The same problem, still open, with new evidence (another file:line, more places, a changed
     cause): `note` on that issue with the new evidence, instead of a new issue.
   - Evidence that an open issue is fixed: `note` that, with the commit or file that shows it. Do
     not close it; whoever owns it does.
   - Unchanged evidence: nothing. No note, no issue.
5. **File what is new** (Phases 4 and 5).
6. **Update the brief**: `get_project`, then `set_project_brief` with its revision, changing the
   state of each area and the target order where they moved. Keep what the owner wrote.
7. `mark_setup_complete` with the commit you swept, and tell the user what is new: issues filed,
   notes added, and what you think is fixed.
