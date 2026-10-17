# Signed approvals

Some changes land only after a person approves them in Nitely: the playbook's `human_paths`,
review flags, a review by the author's own vendor when the playbook requires another. Until now an
approval was an event in the database, `issue_moved … → Approved` by the actor `human`. Agents run
as the same OS user as the app, so any agent with a shell could write that row (on purpose, or by a
confused `sqlite3` call) and its change would land as approved.

This makes an approval something only the app can produce: a signature with a key the app keeps in
the OS keychain, checked by `land` before anything lands.

## What it protects against, and what not

- **Protects:** an approval appearing because something wrote the database (an approval row, a
  signature, a key of its own), edited `config.json`, called the CLI or an MCP tool, or replayed an
  old approval onto a changed branch. None of those can produce a signature that verifies against
  the key in the keychain, so `land` refuses.
- **Does not protect against** an agent that deliberately tampers with the keychain: reads the
  signing key out of it, or replaces the public key there, and forges a signature. Agents run as the same OS user, and on Windows the Credential Manager
  hands any of that user's processes the secret. On macOS the keychain entry is tied to the app, so
  reading it from another program asks the person first. Stopping a determined agent completely
  takes a separate OS account for agents, which is out of scope here.

So the line is between Nitely's own files and the OS keychain: writing the first is easy and can
happen by accident; tampering with the second is a deliberate attack that the person would see in
the agent's transcript.

## The key

- An Ed25519 key pair. The app creates it the first time it starts and finds none.
- The **private key** stays in the OS keychain (service `Nitely`, account `approval-key`), read only
  by the app, only when the person approves something.
- The **public key** is in the keychain too, in its own entry (account `approval-public-key`), which
  holds nothing secret. `land` runs in agents' processes (the CLI, the MCP server) and reads only
  that entry; a key in Nitely's database or files is never trusted.
- The app checks the public entry against its private key when it starts and before every
  approval, and puts its own back (logged as an `approval_key` event) if it was changed.
- **One data folder, one set of entries.** The service `Nitely` belongs to the person's own data
  folder. A process on any other (`NITELY_HOME`: a test, a sandbox, a second profile) uses the
  service `Nitely (<hash of the folder's path>)` for every entry (the key, its public half, the
  paired phones, the push key), so it can neither read the person's key nor sign with it, and its
  approvals never verify against the person's. The test harness opens no keychain entry at all.
  Only `entry` in `approval.rs` opens one, and a test keeps it that way.
- **macOS:** only the app that made a keychain entry reads it without the OS asking the person. So
  on macOS an agent's `land` refuses a change that waits for approval, and the app lands it itself
  once the person approves, whatever the playbook says about who lands. Windows and Linux read the
  public entry silently from any process, and the playbook's landing rule applies as before.

## What is signed

When the person approves an issue in the app, the app signs

```
nitely-approval-v1
<issue key>
<patch id of the review they approved>
<approval time, unix seconds>
```

and stores the signature with the approval (`approval` table: issue, patch, by, time, signature).
The patch id ties the approval to exactly the change that was reviewed. A branch that changes
afterwards has another patch id and needs a new review and a new approval, as before.

## What land checks

`land` (and the app's land-on-approval) already requires a standing approval: the issue Approved by
the human after its latest review of the current patch. It now also requires a signed approval
whose patch is the current one, made at that approval's time, that verifies against the public
key. When it does not:

- no key in the keychain yet: "no approval key yet: open the Nitely app once so it can make one";
- no signature, or one that does not verify: "the approval is not signed by this machine's Nitely
  app; it is back in Needs You: approve it again in the app". The issue moves back to Needs You, so
  the person sees it in the Inbox.

Changes that need no human approval are unaffected. Approvals given before this change carry no
signature: when the app starts, it sends every Approved issue without a valid signature back to
Needs You, with a note, so the person approves it once more in the Inbox.

## Code

- `crates/nitely-core/src/approval.rs`: the message, signing and verifying, the keychain entries (the
  `keyring` crate), and `Db` methods for signed approvals.
- `src-tauri/src/lib.rs`: creating the key, signing when the person moves an issue to Approved (not
  a playbook proposal, which applies at once), and the key check on start and before every approval.
- `crates/nitely-core/src/land.rs`: `check_approval` verifies.
