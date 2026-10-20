# Releasing Nitely

Releases are built by CI ([.github/workflows/release.yml](../.github/workflows/release.yml)) and
published on GitHub Releases. Nobody builds an installer by hand. There are three channels:

The opt-in [exact release verification workflow](release-verification.md) adds a Full gate,
whole-release peer review, all-platform/native checks and installer upgrade checks on a committed
version-stamped candidate. It publishes that candidate's tested archive and delays promotion tags
until verification. It is inactive until owner-approved release policy and dedicated reviewer
authentication are provisioned; the steps below describe the existing default workflow.

| Channel | Comes from | Version | On GitHub | Who gets it |
| --- | --- | --- | --- | --- |
| **stable** | a tag `v0.2.0`, or promoting a beta | `0.2.0` | a release | everyone, by default |
| **beta** | a tag `v0.2.0-beta.1` | `0.2.0-beta.1` | a pre-release | people who choose beta |
| **nightly** | main, every day at 05:00 UTC when it changed (or on demand) | `0.2.0-nightly.20261016.1234.g1a2b3c4` (day, main's commit count, commit) | the rolling pre-release `nightly` | people who choose nightly |

A tag `v0.2.0-test.1` builds everything and publishes a **draft** that only the repo's
collaborators see, with no update manifest: use it to try the workflow, then delete the draft
and the tag.

Which build belongs to which channel is decided in one place,
[scripts/release-meta.mjs](../scripts/release-meta.mjs), and its tests pin the rules.

## The version

main always carries the **next** version to release, in five files that
[scripts/version.mjs](../scripts/version.mjs) keeps in step (`npm test` fails when they differ):

```powershell
node scripts/version.mjs            # prints it
node scripts/version.mjs set 0.3.0  # writes it everywhere
```

A release build stamps its own version (`0.2.0-beta.1`, a nightly's date and commit) without
committing it. After a stable `0.2.0` ships, land a change that sets main to `0.3.0`.

## The Mac

CI builds the Mac app and disk image on every pull request (the "Mac bundle" step of
[ci.yml](../.github/workflows/ci.yml), the `mac-bundle` artifact): `Nitely.app` with the `nitely`
CLI beside the app's own `nitely-app`, unsigned. Releases do not carry it yet: a Mac build people
download must be signed with an Apple Developer ID and notarized, or macOS refuses to open it.

## What one build does

1. Runs the gate (the same four commands as CI).
2. Stamps the version, builds the `nitely` CLI, and builds the per-user NSIS installer, which
   carries the CLI beside the app. After installing, the installer runs `nitely install`. That
   puts the CLI where agents' hooks and MCP servers run `nitely`: over the copy already on PATH, or
   on a machine without Rust into the user's WindowsApps folder, which Windows keeps on PATH.
   Uninstalling removes that copy. The app's own executable stays `nitely-app.exe` (shortcuts say
   "Nitely"): Windows file names ignore case, so an app named `Nitely.exe` would be the CLI.
3. Installs the new installer silently on the runner and checks that the app and the CLI are in
   place and report the right version.
4. Writes the notes with `nitely release-notes`: every issue whose commits landed since the
   previous release (for a stable release, the previous stable one, so the notes cover its betas
   too), with its title and a link to its pull requests. Commits that name no issue are listed
   after them. The very first release lists no history: commits from before Nitely checked them
   for private terms (NIT-80) never reach public notes. Notes on SmartScreen and on checking a
   download follow, since the installer is not code-signed (below).
5. For a tag (stable, beta, test), attests the installer, the CLI and `SHA256SUMS.txt` with
   [actions/attest-build-provenance](https://github.com/actions/attest-build-provenance): a
   provenance statement naming the workflow, commit and run that built each file, signed keylessly
   with the run's GitHub OIDC identity through Sigstore. The nightly, replaced every day, is not
   attested.
6. Publishes the installer, the CLI on its own, and `SHA256SUMS.txt` (each file's SHA-256).
7. With the updater key set (below), signs the installer for the updater and updates the channel's
   manifest. A stable release updates both `stable.json` and `beta.json`.

## Not code-signed: how people check a download

The installer has no Authenticode signature: Nitely is for developers, and a code-signing
certificate (NIT-94) isn't worth its cost and its yearly renewal yet. So Windows SmartScreen warns
the first time someone runs it (**More info → Run anyway**), and the README and every release's
notes say so plainly. What people can check instead, for free and without a key anyone has to keep:

- **The attestation** (tagged releases):

  ```powershell
  gh attestation verify .\Nitely_0.2.0_x64-setup.exe -R frontiermaxxing/Nitely
  ```

  It passes only for a file this repository's release workflow built, and prints the tag and
  commit it came from. Attestations are listed under the repository's **Actions → Attestations**.
- **The checksums**: `SHA256SUMS.txt`, on every release, in `sha256sum` format (`*` marks a binary
  file). `sha256sum -c SHA256SUMS.txt` checks them all in Git Bash; in PowerShell compare
  `(Get-FileHash <file>).Hash` with the file's line. A checksum only shows the download is the file
  the release holds; the attestation also shows who built it.
- **Building from source** (README), for anyone who would rather trust no download at all.

The updater's own signature (below) is separate: it is what installed copies check before they
install an update, and it doesn't make Windows trust the first install.

## Update manifests

Installed copies read their channel's manifest (the Tauri updater format) at a fixed URL:

- `https://github.com/frontiermaxxing/Nitely/releases/download/updates/stable.json`
- `https://github.com/frontiermaxxing/Nitely/releases/download/updates/beta.json`
- `https://github.com/frontiermaxxing/Nitely/releases/download/updates/nightly.json`

They are assets of the pre-release `updates`, which the workflow creates the first time.

## The updater signing key (the maintainer, once)

Auto-update is off until this is done: today the repository has no `TAURI_SIGNING_PRIVATE_KEY`
secret and `tauri.conf.json` has no public key, and every release build says so in a warning
("Updates are off for this build"). The updater accepts an update only when its signature verifies
against a public key built into the app. The private key must never reach the repo, an issue, a
chat, an agent's terminal or a build log, so you make it yourself, and it only ever moves from a
file to GitHub's secret store:

1. On your own machine, in a terminal no agent is running in, outside any repository:

   ```powershell
   npx @tauri-apps/cli signer generate -w $HOME\.tauri\nitely-updater.key
   ```

   Always pass `-w`: without it the private key is printed to the terminal. It asks for a password;
   give it one. It writes `nitely-updater.key` (private) and `nitely-updater.key.pub` (public).
   Back up the key file and the password in a password manager. If the key is lost, installed
   copies can't take updates until they are reinstalled by hand.
2. Store both secrets with the [GitHub CLI](https://cli.github.com), straight from the file, so the
   key is never pasted, typed, or shown:

   ```powershell
   Start-Process gh -ArgumentList "secret set TAURI_SIGNING_PRIVATE_KEY -R frontiermaxxing/Nitely" -RedirectStandardInput "$HOME\.tauri\nitely-updater.key" -NoNewWindow -Wait
   gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD -R frontiermaxxing/Nitely
   ```

   The first hands `gh` the file exactly as written (PowerShell's pipe would add a newline). The
   second prompts for the password without echoing it. Don't pass either with
   `--body`: the value would land in your shell history. (The web page works too: **Settings →
   Secrets and variables → Actions → New repository secret**, pasting the file's contents.)
3. Give an agent the **public** key (the contents of `nitely-updater.key.pub`; it is safe to share)
   to land in `src-tauri/tauri.conf.json` as `plugins.updater.pubkey`.
4. Check the next release build (a test tag, below, is enough): the "Updates are off" warning is
   gone and the release has a `Nitely_*_x64-setup.exe.sig` beside the installer.

The workflow hands the secret only to the step that builds the installer, as environment
variables GitHub masks in the log; no step prints it. `.gitignore` ignores `*.key` as a backstop.
If the private key ever leaks, make a new pair the same way and land the new public key: copies
installed with the old one then need one install by hand.

Until both halves are in place (the secret, and the public key in `tauri.conf.json`), releases
still publish installers; they just carry no updater signature, no manifest changes, and the app
they install doesn't check for updates (Agent setup → Updates says so). Copies installed before
the key existed don't update themselves: install a later release over them once, by hand. A
manifest only ever moves forward: no build writes its own version; each writes the newest
published release of the channel (with its installer's signature), from the releases as they
are then, and checks again three times over the next minute. Builds finishing side by side write
the same thing, and one that saw the list before a newer release was published is put right by a
later check.

## How installed copies update

The app checks the manifest of the channel it follows (Agent setup → Updates: stable by default
on a stable build, the build's own channel otherwise) when it starts and every four hours. It
verifies the update against the public key, including the version it was signed for, and downloads
it in the background. The sidebar then shows "Restart to update"; it installs then, or when Nitely
quits, and never while a land runs (it waits for every land to finish). The installer puts the new
`nitely` CLI in place the way `nitely install` does, so agents in the middle of a call keep the
copy they started with, and the crew keeps running. Builds from source (`npm run tauri dev`,
`cargo build`) never check: their channel is `dev`. `nitely --version` prints the channel.

Moving to a more settled channel (nightly to beta or stable) asks first: its newest version can be
older than the build you run. An older build refuses a database a newer one upgraded and says which
version to install (`nitely.db.upgraded-by` beside the database records it).

## Cutting the first stable release (Andrew)

1. **Check main.** CI is green on main, and `node scripts/version.mjs` prints the version you mean
   to release (`0.1.0` today).
2. **Optional: set up the updater key** (above), so this release is the first that later releases
   can update.
3. **Ship a beta first.** Tag main and push the tag. Nothing else is needed:

   ```powershell
   git checkout main; git pull
   git tag v0.1.0-beta.1
   git push origin v0.1.0-beta.1
   ```

   Watch **Actions → Release**. In about 20 minutes the pre-release `v0.1.0-beta.1` has the
   installer, the CLI, checksums and notes. Install it on a clean user account or another PC, and
   connect a repo.
4. **Promote it.** **Actions → Promote a beta → Run workflow**, beta tag `v0.1.0-beta.1`. It tags
   that commit `v0.1.0` and starts its build as the stable release (a second run under **Actions →
   Release**, on the tag, so it is attested like a pushed tag). Found a problem in the beta? Fix it
   on main and tag `v0.1.0-beta.2` instead.
5. **Move main on.** Have an agent land `node scripts/version.mjs set 0.2.0`, so nightlies
   carry the next version.

To build a tag again (say the build failed for a reason outside the code), run the workflow from
that tag, with the tag as `ref` too:

```powershell
gh workflow run release.yml -R frontiermaxxing/Nitely --ref v0.1.0 -f ref=v0.1.0
```

(In **Actions → Release → Run workflow**, choose the tag under "Use workflow from".) A run from a
branch builds the tag too but publishes it unattested, since its attestation would name the
branch's commit. A release already published under the tag has to be deleted first.

## Trying the workflow

Push a test tag from any commit (a branch's, to try a change to the workflow); it publishes only a
draft, attested like a real tag's:

```powershell
git tag v0.1.0-test.1; git push origin v0.1.0-test.1
# after checking the draft on GitHub (its files, and gh attestation verify on one of them):
gh release delete v0.1.0-test.1 --yes --cleanup-tag
```
