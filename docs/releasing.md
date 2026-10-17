# Releasing Nitely

Releases are built by CI ([.github/workflows/release.yml](../.github/workflows/release.yml)) and
published on GitHub Releases. Nobody builds an installer by hand. There are three channels:

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
   for private terms (NIT-80) never reach public notes. A note on SmartScreen follows, since the installer is not code-signed yet
   (NIT-94).
5. Publishes the installer, the CLI on its own, and `SHA256SUMS.txt`.
6. With the updater key set (below), signs the installer for the updater and updates the channel's
   manifest. A stable release updates both `stable.json` and `beta.json`.

## Update manifests

Installed copies read their channel's manifest (the Tauri updater format) at a fixed URL:

- `https://github.com/frontiermaxxing/Nitely/releases/download/updates/stable.json`
- `https://github.com/frontiermaxxing/Nitely/releases/download/updates/beta.json`
- `https://github.com/frontiermaxxing/Nitely/releases/download/updates/nightly.json`

They are assets of the pre-release `updates`, which the workflow creates the first time.

## The updater signing key (Andrew, once)

The updater accepts an update only when its signature verifies against a public key built into
the app. The private key must never be in the repo, in an issue, or in front of an agent, so you
make it yourself:

1. On your own machine, in a terminal (not one an agent is running in):

   ```powershell
   npx @tauri-apps/cli signer generate -w $HOME\.tauri\nitely-updater.key
   ```

   Give it a password. Back up the key file and the password somewhere safe (a password manager).
   If the key is lost, installed copies can't take updates until they are reinstalled by hand.
2. On GitHub: **Settings → Secrets and variables → Actions → New repository secret**. Add
   `TAURI_SIGNING_PRIVATE_KEY` (the contents of `nitely-updater.key`) and
   `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
3. Give an agent the **public** key (`nitely-updater.key.pub`; it is safe to share) to land in
   `src-tauri/tauri.conf.json` as `plugins.updater.pubkey` (empty until then).

Until both halves are in place (the secret, and the public key in `tauri.conf.json`), releases
still publish installers; they just carry no updater signature, no manifest changes, and the app
they install doesn't check for updates (Agent setup → Updates says so). Copies installed before
the key existed don't update themselves: install a later release over them once, by hand. A
manifest only ever moves forward: a build that finishes after a newer one leaves it alone, and
when two finish within seconds of each other, the newer one reads its upload back for a minute
and puts it back if the older one overwrote it.

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
   that commit `v0.1.0` and builds it as the stable release. Found a problem in the beta? Fix it
   on main and tag `v0.1.0-beta.2` instead.
5. **Move main on.** Have an agent land `node scripts/version.mjs set 0.2.0`, so nightlies
   carry the next version.

To build a tag again (say the build failed for a reason outside the code): **Actions → Release →
Run workflow**, with that tag as `ref`. A release already published under the tag has to be
deleted first.

## Trying the workflow

Push a test tag from any commit; it publishes only a draft:

```powershell
git tag v0.1.0-test.1; git push origin v0.1.0-test.1
# after checking the draft on GitHub:
gh release delete v0.1.0-test.1 --yes --cleanup-tag
```
