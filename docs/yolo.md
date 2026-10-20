# Project YOLO mode

Project Settings has a **YOLO mode** switch. It is off by default and applies to all repositories
connected to that project. Only the person can change it in the app; agents can read its state.

When enabled, automatic submission and landing skip the project's gate commands and automated
PR review. This includes landing batches. Skipped checks are recorded as skipped, with no
successful test or reviewer result. PRs are still created, and normal Git conflict handling,
blocked paths and explicit human approval requirements still apply. Hosted CI is configured
separately and is not disabled by this switch.

Manual development checks remain available. Turning YOLO off restores each repository's
configured checks; a revision submitted without them cannot use its YOLO record as a normal
gate or review pass. Already running processes are not forcibly killed when the switch changes.

The setting stays in Nitely's project database, rather than editing repository playbooks.
Changing it records who changed it and when. Concurrent edits from an older settings window
are refused until that window reloads the current value.
