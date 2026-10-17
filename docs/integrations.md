# GitHub, Bitbucket and Linear

What Nitely syncs from code hosts and trackers, and the one-time move off Linear. Back to the [README](../README.md).

## GitHub

Watched repos whose `origin` is on GitHub get their pull requests, approvals and CI checks synced,
with no setup beyond a signed-in GitHub CLI (`gh auth login`). Nitely uses `NITELY_GITHUB_TOKEN`
or `GITHUB_TOKEN` instead when set. A PR shows on the issue its branch names (`claude/nit-7-...`).


## Bitbucket and Linear

Nitely reads `credentials.json` in its data folder (`%APPDATA%\Nitely` on Windows; `nitely where`
prints it) and never writes it:

```json
{
  "bitbucket": { "email": "you@example.com", "apiToken": "…", "workspace": "acme", "repo": "my-app" },
  "linear": { "apiKey": "lin_api_…", "projectId": "…" }
}
```

Either section can be left out. Nitely reads no other location: if you kept this file somewhere
else before, move it here (or point `NITELY_CREDENTIALS` at it).

The app syncs every 60 seconds and on "Sync now"; `nitely sync` does the same from a terminal and prints what it got. Both are read-only on Bitbucket and Linear. Pull
requests come from each watched repo's Bitbucket `origin` plus the repo named in the credentials
file; issues are the open ones in the Linear project the file names. To leave Linear, `nitely
import-linear --project APP` moves a Linear project into native issues once, keeping its keys; it
shows what it would do until you add `--yes`, and `--state "Needs Review=Needs You"` chooses the
Nitely state for a Linear state Nitely has no name for (an open state named "Needs …" is Needs You
unless you choose otherwise). To keep such a choice for the Linear mirror
too, put it in `config.json` in the data folder: `"linear_states": { "Needs Review": "Needs You" }`.
