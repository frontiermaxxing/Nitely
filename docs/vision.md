# Nitely: where it is going

**Status:** Claude's proposal, 2026-10-16, after Andrew described the end goal. Andrew decides the
open questions at the bottom. `design.md` covers milestone 1 and stays accurate for that.

## The product in one line

Nitely is a local desktop app where one person runs a team of coding agents: they connect Claude
Code, Codex and Cursor in a click, and Nitely gives those agents the backlog, the instructions and
the guardrails to work autonomously, and gives the person one place to see the work and make the
calls only a human should make.

## Three parts

**1. Work: projects and the backlog (replaces Linear).**
A project is a backlog, one or more repos, and a playbook. Each project has its own issue prefix
(`NIT-12`, `AND-268`). Issues carry kind, source, evidence and history, the vocabulary the Aera
scripts already enforce, so the habits that made agents file useful issues carry over. Linear is a
one-time import, not an integration we maintain.

**2. Agents: connect once, then they just work.**
"Connect Claude Code" in Nitely writes the hooks, registers the `nitely` MCP server, and adds the
instruction pack (a short `AGENTS.md`/`CLAUDE.md` section and skills) to each repo of the project.
The MCP server is the main contract, because it is what makes agents autonomous rather than just
observed:

- `next_task`: the highest-priority ready issue this agent may take, claimed atomically so two agents
  never start the same one
- `start_task`: makes the worktree and branch per the playbook
- `report` / `note` / `ask`: status, handoff notes, a question that lands in Andrew's inbox
- `submit`: runs the playbook's review and gates and opens the pull request
- `file_issue`: with the duplicate check and evidence rule

Hooks stay as the passive layer, so even an agent that ignores the instructions still shows up.

**3. Flow: the playbook (generalized `work.mjs`).**
What `start`, `pr` and `land` do for Aera becomes a per-project playbook Nitely runs itself:
integration branch, worktree layout and setup commands, gate commands and the known-failures
baseline, which vendor reviews which, approval rules (paths and reviewer flags that need a human),
and how landing works (serial, under a lock, rebase then gate). Aera's playbook is the first one,
written from its scripts. A new user starts from a template ("GitHub, single branch, tests must pass,
Codex reviews Claude") and edits it in the app.

## What the person sees

- **Inbox:** everything waiting on a human, each with its action in place: approve, answer, unblock.
  This is the screen that justifies the app.
- **Now:** live agents and tasks across projects (what is built today, grown up).
- **Project:** backlog, board by state, and the branch and worktree picture.
- **Task:** the full timeline of one issue: agent turns, commits, review rounds and findings, gate
  results, PR, land.
- **Playbook:** the project's rules, editable, with a dry run.

Quality bar: keyboard-first with a command palette, instant search, live updates pushed from file
watchers and the event store rather than polling, a real design system, light and dark. Linear is the
reference for polish.

## Architecture changes from milestone 1

- **Nitely becomes the source of truth and the runner, not only an observer.** The playbook engine
  lives in `nitely-core`; the CLI and MCP server are thin front ends over it. Aera's `work.mjs` shrinks to
  calling `nitely` and is retired once the Aera playbook covers it.
- **A provider trait for code hosts:** pull requests, checks, reviews, merge. GitHub and Bitbucket are
  real integrations (GitHub matters most for the open-source audience; Bitbucket for Aera).
- **Projects in the data model,** with repos, keys and playbooks per project.
- **Approvals stay human-only** and become verifiable: signed by a key the app holds in the OS
  keychain, checked before landing, so an agent with shell access cannot forge one.
- **Local-first, no server.** An optional sync service is a later product decision, not a
  requirement. The one place this matters now is cloud agents (like this thread), which cannot reach a
  database on Andrew's PC; see question 3.

## How I would build it

Dogfood from the next milestone: the Nitely project tracks its own work in Nitely, and every gap I
hit using it becomes an issue in it. Order:

1. **Projects and native issues, plus the MCP server** (`next_task`, `report`, `ask`, `file_issue`).
   From here Nitely's own backlog lives in Nitely.
2. **Inbox and task timeline,** and the UI foundation (design system, palette, push updates). This is where
   it starts to feel like a product.
3. **Playbook engine** with the Aera playbook; `start`, `submit` and `land` run from Nitely.
4. **GitHub provider,** Linear import, Cursor and Codex connectors, then the onboarding flow and a
   public repo.

I'll keep `design.md` and this file current as decisions land, write the first Nitely issues from
this list, and ask only for what changes the goal or cannot be undone.

## Open questions for Andrew

1. **Repo:** make a private GitHub repo for Nitely now (public later), so its own pull requests go
   through Nitely? Recommended: yes.
2. **Workflow:** move the `work.mjs` workflow into Nitely as the playbook engine, with Aera as its first
   playbook? Recommended: yes.
3. **Where I work:** build Nitely through a Claude Code session on your PC (Remote Control) instead of
   from the cloud, so I show up in Nitely like any other agent and use its MCP server for real?
   Recommended: yes, from step 1 on.
