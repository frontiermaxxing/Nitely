# Agents on other machines

Letting agents on another computer on your network use this machine's backlog. Back to the [README](../README.md).

## Agents on other machines on your network

The backlog lives in the database on the machine that runs Nitely. `nitely serve` lets agents on
other machines on the same local network (a Mac beside a Windows PC, say) reach it: it is the
`nitely` MCP server over HTTP at `/mcp`, so Claude Code there needs no Nitely install.

1. In the app, open **Settings**, **Other machines** and add one (say `mac`, default project APP). It shows that
   machine's token once, inside the command to run there, with a Copy button.
2. Run `nitely serve` on this machine. It listens on port 7421 and announces itself on the network.
3. On the other machine, run the copied command:

```bash
claude mcp add --transport http nitely "http://192.168.1.68:7421/mcp" --header "Authorization: Bearer nly_..."
```

`nitely discover` (or `dns-sd -B _nitely._tcp` on a Mac without Nitely) finds this machine's address.

- **Local network only.** Requests from addresses outside the machine's own networks (private
  ranges, link-local, or the same subnet as one of its interfaces) are refused, so a forwarded port
  never exposes the backlog. It speaks plain HTTP: keep it to a network you trust.
- **A token per machine, made only in the app.** It is shown once, to you; no command an agent can
  run prints or stores a token, and Nitely keeps only its hash. `nitely tokens list` shows names and
  projects, and removing a machine (in the app, or `nitely tokens remove mac`) cuts it off. Its
  agents act as `claude@mac`, and the token's project is the default for their calls.
- **The backlog, not the repo.** Remote agents get `next_task`, `list_issues`, `get_issue`,
  `file_issue`, `update_issue`, `report`, `note`, `ask`, `get_answer` and the project brief tools. `start_task`, `run_gate`,
  `submit` and `land` work in a repo on the machine that runs them, so they are not offered.
  Approving stays in the app, as everywhere.
- **Firewall.** The first time `nitely serve` listens, Windows may ask whether to allow it on private
  networks; allow it there, and only there.
- `nitely discover` checks each announced address before listing it, so what it prints answers.
- Remote agents show on Now under "On other machines", and count among the agents working on Pulse.
- The server is small on purpose: one thread per connection (32 at most, 4 per address), one
  10-second deadline per request however slowly it arrives, a
  Content-Length body of at most 1 MB, and the connection closed after each answer.
