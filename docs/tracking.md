# Tracking projects

Nitely runs a project in one of two ways. You choose when you add the project, and you can switch
in its **Settings → Review mode** at any time.

- **Development** (the default) is for code. Each change is tested and reviewed before it lands,
  on one of the review modes in [review-modes.md](review-modes.md): Fast, Careful, Release or
  Custom.
- **Tracking** is for work that has nothing to test: research, writing, notes, plans, a repo of
  documents. Nitely coordinates the agents and checks nothing. A change lands as soon as an agent
  hands it in.

## What stays the same

Agents keep the whole coordination flow: the board, `next_task` and claims, a branch and a light
worktree per issue, `report`, notes and handoffs, `ask`, `triage`, the crew and goals. When two
agents change the same lines, the second one's land stops on the rebase conflict and the issue goes
back to that agent, as in Development.

Your own rules still hold. A change to a path your playbook's `[[approval.human_paths]]` sends to
you waits for you in the Inbox, and `[[approval.blocked_paths]]` still refuse a branch.

## What Tracking turns off

`submit`, `land` and `quick_land` run no gate, no review, no affected-test selection, no audits,
and no full suite on main afterwards. There is no pull request: the change is rebased onto the
integration branch, pushed and the issue closed, in the same call. The quick path has no size limit.
Review flags in the playbook are not asked, since no reviewer runs.

The instructions agents are served (the MCP instructions, the skill and the session-start text)
describe this flow: no testing standard, no review rounds. Agents can read the mode with
`get_project` but never change it. Only you set it, with the same revision checks as any review
mode, so two windows cannot overwrite each other.

A new Tracking project's generated playbook leaves out the `always_human` line that holds every
change of a new Development project until its review rules are approved.

## Switching

A switch applies to the next submit and land. Work queued under Development when you switch to
Tracking lands without the gate and review it was waiting for. Switching back to Development starts on
Fast (or, before you save, the preset you had picked); choose another preset there if you want one.
The next submit runs its gate and review again. Nothing that already landed is checked again.

## Which to use

Use Development whenever a broken change would cost you: code, configuration, anything a build or a
test can check. Use Tracking when you want the agents organised and their work recorded, but every
handed-in change is acceptable as it is, or you read the results yourself.
