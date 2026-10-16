# The issue mirror

Nitely's issues live on this machine. People who look at the repo see pull requests appear with no
issue saying why. The mirror publishes a project's issues to its repo's own tracker, GitHub Issues
or Bitbucket's issue tracker, so the work is visible where people already look.

It is one way. Nitely stays the source of truth: nothing said on the host is read back, and a
comment there saying "approved" means nothing (approvals are signed in the app, docs/approvals.md).

## Turning it on

Off by default, per project. Only the person turns it on, in the project's settings in the app
(`set_mirror`; an agent's call is refused). The card says where it would publish, or why it cannot
(no GitHub or Bitbucket origin, issues turned off on the repo, a token that cannot write issues),
and how many issues each choice would publish now:

- **only issues filed from now on** (the default),
- **also the open issues there are now**,
- **also every issue there is now, closed ones too**,

with a preview of how one would look. Publishing starts after a confirmation. `nitely mirror status`
shows the same from the CLI; it cannot turn the mirror on.

## What goes out

- The title, the body, the evidence, and a footer: "Tracked in Nitely as NIT-12 (state, kind,
  priority). Nitely is where it is decided; comments here are not read."
- Labels: the kind, `priority: high` and so on, and the issue's own labels. Bitbucket has no
  labels, so they go in the text, and the kind and priority use its own fields.
- Open or closed: Done closes as completed; Canceled and Duplicate close as not planned.
- Comments for the timeline entries an outsider can follow: handoff notes, review outcomes (with
  the pull request), lands, and Done or Canceled with the reason. Not status lines. Comments start
  when an issue is first mirrored; a backfill does not replay old history.
- Pull requests that `submit` opens start with `Closes #12`, so the host links the two.

## What stays private

Before anything leaves the machine:

- Local paths become repo-relative (`src/lib.rs`) or `<local path>`; agent ids such as
  `claude:c:/code/app-worktrees/app-3` become `claude`.
- The text goes through the private terms (private_terms.rs). An issue whose text holds one is
  **held back**, and a comment that holds one is skipped. The app says which entry, never the term.
- These are **private** and never published: an issue the person marked private (the issue panel's
  "Keep private"), the human's own work (owner human), anything labelled `security`, and an urgent
  (P1) bug, which may be a security problem until someone says otherwise.

An issue that becomes private after it was published is **withdrawn**: its host issue is deleted
when the token may (admin rights on GitHub). Otherwise its comments are deleted, its title and text
are replaced with a note, and it is closed as not planned. Hosts keep an edited text's earlier
versions, so the issue panel then says to delete the issue on the host to remove them. The issue panel shows each issue's place:
its link on the host, or private, held back or withdrawn, and why.

## Running

The app mirrors in the background with its other syncs, at most 30 host writes a round, so a big
backfill goes out over a few minutes, within the hosts' limits, and a slow host never holds up
filing an issue. `nitely mirror sync` runs a round now. Each issue keeps its host number and URL
and a fingerprint of what was last sent, so only changes go out.

## Code

`crates/nitely-core/src/mirror.rs`: the hosts (GitHub and Bitbucket behind one `Host` interface),
what goes out and what stays private, the preview and the sync. `src/Mirror.tsx`: the settings card
and the issue panel's line.

## Later

Bringing host comments back into Nitely as notes from outside, which agents treat as data, not
instructions; and closing or reopening in Nitely when someone does it on the host.
