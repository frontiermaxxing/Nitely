# nitely-leaderboard

The Leaderboard service: a small program that receives the weekly reports Nitely installs send
after their owners opt in, and serves the public board. It is one binary and one SQLite file.

- **Signed, no accounts.** Each install makes an ed25519 key when it joins. The public key is its
  only id. Every report and delete is signed with that key. There are no accounts, emails or
  GitHub sign-ins.
- **Counts and rates only.** It accepts a report only in the exact schema Nitely builds
  (`crates/nitely-crew/src/leaderboard_report.rs`). Unknown fields and out-of-range numbers are
  refused, so a report can't carry anything else.
- **No addresses.** It never reads, logs or stores the address a request came from, or any
  header a proxy adds with one (`CF-Connecting-IP`, `X-Forwarded-For`, `X-Real-IP`). The one line
  it prints is where it listens. Turn off your proxy's access logs too (below). Your proxy and any
  CDN in front of it still see the sender's address, so the opt-in page doesn't claim otherwise.
- **Retention.** It keeps one report per install per ISO week and drops reports after 53 weeks.
  A signed delete removes everything an install sent, and leaving in the app sends one.

## Build and run

```sh
cargo build --release -p nitely-leaderboard
./target/release/nitely-leaderboard --listen 127.0.0.1:8787 --db /var/lib/nitely-leaderboard/leaderboard.db
```

| Option | Environment | Default |
| --- | --- | --- |
| `--listen ADDR` | `NITELY_LEADERBOARD_LISTEN` | `127.0.0.1:8787` |
| `--db FILE` | `NITELY_LEADERBOARD_DB` | `leaderboard.db` |
| `--base PATH` | `NITELY_LEADERBOARD_BASE` | none |

Its endpoints are under `/v1/leaderboard/`, so it can share a host with other services.
`--base` is a path prefix your proxy passes through: if it forwards
`https://example.com/api/v1/leaderboard/...`, either strip `/api` in the proxy or run with
`--base /api`.

Keep it listening on loopback and put a TLS proxy in front of it. The app sends reports only to
`https://` addresses (or `http://` to 127.0.0.1 while you develop).

To point Nitely at your instance, run `nitely leaderboard join --url https://example.com ...`
or set `NITELY_LEADERBOARD_URL`.

### systemd

```ini
[Unit]
Description=Nitely Leaderboard
After=network.target

[Service]
ExecStart=/usr/local/bin/nitely-leaderboard --listen 127.0.0.1:8787 --db /var/lib/nitely-leaderboard/leaderboard.db
DynamicUser=yes
StateDirectory=nitely-leaderboard
Restart=on-failure
# It prints only where it listens; keep the journal from holding anything else anyway.
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

## Behind a proxy, with access logs off

nginx:

```nginx
server {
    listen 443 ssl;
    server_name example.com;
    # ssl_certificate ...;

    location /v1/leaderboard/ {
        access_log off;
        proxy_pass http://127.0.0.1:8787;
        # Pass no address headers on: the service ignores them, and they shouldn't travel.
        proxy_set_header X-Forwarded-For "";
        proxy_set_header X-Real-IP "";
        proxy_set_header CF-Connecting-IP "";
        client_max_body_size 64k;
    }
}
```

Caddy:

```caddy
example.com {
    handle /v1/leaderboard/* {
        reverse_proxy 127.0.0.1:8787 {
            header_up -X-Forwarded-For
            header_up -CF-Connecting-IP
        }
    }
    # Caddy writes no access log unless a `log` directive asks for one: leave it out.
}
```

If a CDN sits in front, turn off its request logging or log sampling for this path if it allows,
and cache `GET /v1/leaderboard/board/*` for a few minutes. The service already sends
`Cache-Control: public, max-age=300` there and `no-store` everywhere else.

## The API

| Request | Signed | Answer |
| --- | --- | --- |
| `GET /v1/leaderboard/health` | no | `{"ok": true, "schema": 1}` |
| `GET /v1/leaderboard/board/{week,30d,all}.json` | no | the board for that period |
| `POST /v1/leaderboard/reports` | yes | `{"ok": true, "week": "2026-W43"}`; a second report the same week replaces the first |
| `DELETE /v1/leaderboard/entries` | yes, empty body | `{"deleted": n}` |

A signed request carries three headers:

- `X-Nitely-Key`: the public key, base64url without padding.
- `X-Nitely-Time`: Unix milliseconds. It must be within 5 minutes of the service's clock and later
  than the key's last request, so a request can't be replayed.
- `X-Nitely-Signature`: base64url ed25519 over the message below, where the path is the one
  after `--base`:

  ```text
  nitely-leaderboard-v1\n{METHOD}\n{path}\n{key}\n{time}\n{hex sha256 of the body}
  ```

A board looks like this:

```json
{
  "schema": 1,
  "period": "week",
  "week": "2026-W43",
  "generated_at": 1792540800,
  "entries": [
    {
      "id": "<public key>",
      "display_name": "Night Owl",
      "links": { "github": "https://github.com/octocat" },
      "projects": 2,
      "week": "2026-W43",
      "landed": 14,
      "landed_per_day": 2.0,
      "reviewed": 12,
      "first_pass_rate": 0.833,
      "lead_time": { "median": 5400, "p90": 14400, "count": 14 },
      "agent_hours": 31.5,
      "tokens": 18200000,
      "inbox_reply": { "median": 900, "p90": 3600, "count": 6 },
      "steadiest_week": { "landed": 14, "active_days": 6 },
      "vendors": [{ "vendor": "claude", "landed": 9, "share": 0.643 }],
      "review_rounds": { "mean": 1.2, "max": 3 },
      "weekly": [0, 3, 8],
      "records": { "best_day": 5, "best_week": 22, "longest_streak_days": 9, "fastest_lead": 840 }
    }
  ]
}
```

- `week`: the newest week with reports, from each install's report for that week.
- `30d`: each install's latest report from the last five weeks.
- `all`: each install's latest report.

Entries are sorted by `landed`. The app re-ranks them by whichever column you choose.

## Tests

`cargo test -p nitely-leaderboard` runs the service against Nitely's own client on a local
instance. The tests cover join, send, view and leave, refusals of unsigned, replayed, tampered
and stale requests, and that no address reaches the database.
