# Deciding from your phone

Agents wait on the person: an ask they must answer, a change they must approve before it lands.
When the person is away from the PC, that wait is dead time. This lets a phone the person paired
with the app answer those asks and approve or send back those changes, and nothing else.

It builds on signed approvals (docs/approvals.md): an approval made on the phone is signed by a key
that never leaves the phone, and `land` checks that signature as it checks the app's own.

## What the phone can do, and what not

- **Can:** see the decisions waiting on the person (asks and approvals), answer an ask, approve a
  change, send a change back to the agents with a reason. Read what happened since they last read
  it on the phone (the last day the first time, two weeks at most), by goal: what landed, what is
  at risk, when each goal will be done, and every place they were needed. It is folded under the
  decisions and reads again with them (signed `POST /phone/api/report` with `since`, NIT-247).
- **Cannot:** anything else. No issue edits, no filing, no settings, no rules, no code, no other
  view of the backlog. The server the phone talks to has no other routes.
- Other kinds of decision in the desktop Inbox (your own work, a stopped land, a conflict) show on
  the phone as a count of "waiting at the PC", because acting on them needs the PC.

## The pieces

1. **The phone server** runs inside the Nitely app, not in `nitely serve`. The app is the person's
   process: it already holds the approval key, and the phone's actions are the person's. It is off
   until the person turns it on in the app (Settings, Phone), and it stops when the app
   quits. It listens on its own port (7422), separate from `nitely serve`'s MCP port.
2. **The phone page** is a small web app the phone server serves (`/phone/`), built from this repo
   like the desktop UI (`phone.html`, a second Vite entry) and embedded in the app. The phone
   needs no install: the person opens it from the pairing QR code and can add it to the home screen.
3. **The device list** of paired phones and their public keys, kept in the OS keychain and signed by
   the app's approval key.

## Choosing where it listens

The phone page carries the code that signs with the phone's key, so the path that delivers it must
be one nobody in between can change. Over plain HTTP on a LAN, anyone who can intercept traffic
there (a hostile device on the Wi-Fi spoofing ARP, say) could replace the page with one that sends
the key away or signs approvals the person never made. Request signatures cannot help when the
signing code itself is replaced. So the phone server serves only over a path that is encrypted and
authenticated end to end:

- **This machine's Tailscale address**: one the Tailscale client itself reports (`tailscale ip`),
  not merely an address in its ranges (`100.64.0.0/10` is also used by carriers' NAT). WireGuard
  encrypts and authenticates every packet between the person's own devices, so plain HTTP inside it
  is safe from the network in between. Other tunnels can follow once Nitely can tell they are one.
  It works on the same Wi-Fi as well as away from home: both devices only need to be on the
  person's tailnet.
- **HTTPS** with a certificate the phone already trusts. TLS authenticates the page whatever network
  carries it, so over HTTPS any of this machine's addresses may serve, a plain LAN address included.
  The Phone screen offers two certificates:
  - **Tailscale's certificate for this machine.** The app runs `tailscale cert` for the machine's
    tailnet name (`pc.tail1234.ts.net`, from `tailscale status --json`), a real Let's Encrypt
    certificate every phone trusts. Nitely reads it and its key from the command's output and keeps
    them only in memory; Tailscale keeps and renews its own copy. The person
    must first turn on **MagicDNS** and **HTTPS Certificates** in the Tailscale admin console (DNS
    page); until then the Phone screen shows Tailscale's own reason. The name resolves only inside
    the tailnet, so the phone needs Tailscale too.
  - **The person's own certificate**: PEM certificate and key files, and the name on the
    certificate. The phone must resolve that name to this machine and trust the certificate's
    authority (a public one, or a local authority the person installed on the phone). Nitely makes
    no certificate authority and runs no server.

  A server left running picks up a renewed certificate by itself: it asks Tailscale again every 12
  hours (Tailscale renews a month before expiry), and checks the person's files every 10 minutes,
  reading them again when they change. A fetch that fails keeps the certificate being served.

Plain HTTP on an ordinary LAN address is refused, and the app says why. Loopback is offered for
trying it on the PC itself.

The phone server never binds `0.0.0.0`. When the person turns it on, the app lists this machine's
addresses that qualify and the person picks the ones to listen on. Only those are bound, and only
clients on those networks are answered (as `nitely serve` does). Turning it off, or quitting the
app, closes the sockets. Over HTTPS the pairing QR carries `https://<name>:7422/phone/`, the name on
the certificate, rather than an address.

The phone keeps its key in storage scoped to the page's origin, so a phone paired over
`http://100.101.7.42:7422` is a stranger to `https://pc.tail1234.ts.net:7422`: after switching, pair
it again (and remove the old entry).

## Pairing

1. In the app the person presses **Pair a phone**. The app makes a **pairing offer**: a random
   32-byte secret, single use, valid for 10 minutes, held only in the app's memory (never written to
   the database or a file). The app shows a QR code for
   `http://<chosen tunnel address>:7422/phone/#pair=<secret>`. The secret is in the URL's fragment, which a
   browser does not send in requests or `Referer` headers.
2. The phone opens the page. It makes an **Ed25519 key pair** on the phone (randomness from
   `crypto.getRandomValues`; the private key is stored in the page's own storage on the phone and is
   never sent anywhere), asks the person for a name ("Pixel", "Andrew's iPhone"), and posts
   `{secret, name, public key, signature}` to `/phone/api/pair`, the signature over
   `nitely-pair-v1\n<secret>\n<public key hex>`, proving it holds the key it names.
3. The phone server checks the secret in constant time against the open offer, then **closes the
   offer**, whatever happens next: a secret pairs at most one phone attempt.
4. The app asks the person: *Pair "Pixel"? The phone shows 4821-0937.* The code is derived from the
   secret and the phone's public key (the first 8 digits of SHA-256 of both), so the phone shows the
   same code without being told it. Only when the person confirms in the app does the phone join the
   device list. Declining, or not answering within the offer's 10 minutes, pairs nothing.
5. The phone polls `/phone/api/pair/status` (signed with its new key) until it is paired, then shows
   the Inbox.

## The device list

Kept in the OS keychain (service `Nitely`, account `phone-devices`; another data folder has a
service of its own, see [approvals.md](approvals.md#the-key)), as JSON:
`{devices: [{id, name, public_key, paired_at}], signature}`, the signature by the app's approval
key over the canonical list. The app writes it when the person confirms a pairing or revokes a
device; the phone server and `land` read it and verify its signature with the approval key's public
half (docs/approvals.md) before trusting any device in it. A device written into the list by anything
but the app does not verify; nothing in Nitely's database or files says which devices are trusted.

**Revoking** a device in the app removes it from the list and re-signs the list. From then on its
requests are refused and its approvals do not land, including approvals it gave before it was
revoked that have not landed yet: a stolen phone's approvals stop counting the moment the person
revokes it. Those issues go back to Needs You at the next land attempt.

## Requests

Every request after pairing carries three headers: the device id, the time (unix seconds) and a
signature by the device's key over

```
nitely-phone-v1
<method>
<path>
<time>
<SHA-256 of the body, hex>
```

The server answers only when the device is in the verified device list, the time is within 60
seconds of the server's clock, the signature verifies, and that signature has not been seen in the
last two minutes (the server remembers recent ones). A captured request cannot be replayed later or
changed in transit. The server answers in JSON and sends `Cache-Control: no-store`.

## Approving, answering, sending back

- **Approve:** the phone shows the patch id of the review the person is looking at, and signs the
  same message the app signs (docs/approvals.md) with its own key and its own time:
  `nitely-approval-v1\n<issue key>\n<patch id>\n<time>`. The server checks it (as a request, and
  that the patch is still the issue's latest review), moves the issue to Approved as the person, and
  stores the approval with the device id and the signed time. `land` accepts it when the device is in
  the verified device list, the signature verifies with that device's key, and the signed time is
  within two minutes of the approval. A changed branch has another patch id and needs a new review
  and a new approval, as before.
- **Answer an ask:** the phone signs `nitely-answer-v1\n<ask id>\n<answer>\n<time>`; the server
  answers the ask as the person and records which device answered.
- **Send back:** the phone signs `nitely-sendback-v1\n<issue key>\n<reason>\n<time>`; the issue goes
  back to Todo with the reason, as **Back to Todo** does in the desktop Inbox.
- **Accept a review finding** (docs/approvals.md, "Accepting one review finding"): an ask that
  carries a finding comes with the patch id of the issue's latest review. **Accept** signs the same
  `nitely-accept-v1` message the app signs, with the person's one-line reason sent exactly as the PC
  keeps it (`POST /phone/api/accept`); **Send back** signs `nitely-answer-v1` over the reason
  (`POST /phone/api/decline`). An acceptance counts at `land` like a phone's approval: while the
  phone is in the verified device list, signed within two minutes of when it was recorded.

An agent on the PC cannot do any of these: it would need a key that exists only on the phone, or
the app's approval key to add a device of its own.

## Threat model

- **A stolen pairing QR** (photographed, screenshotted, read over a shoulder): the secret alone
  pairs nothing. The person confirms each pairing in the app after comparing the code on both
  screens; the offer is single use and lasts 10 minutes. If someone else uses it first, the person's
  own phone fails to pair and the app shows a request from a name they did not choose, which they
  decline.
- **Pairing replay:** the secret closes on first use; a replayed pairing request finds no offer.
- **Request replay or tampering on the network:** each request is signed with its time and body,
  refused outside a 60-second window, and refused when its signature was already seen.
- **Someone on the same network reading or changing traffic:** the phone server speaks only inside
  an encrypted, authenticated tunnel or over HTTPS with a certificate the phone trusts, so they can
  neither read the decisions nor replace the page that holds the key ("Choosing where it listens").
- **A stolen certificate key:** whoever holds it and can reach the phone on that name could serve a
  page of their own. Nitely writes no key file: Tailscale's key stays in Tailscale's own state, and
  the person's own key wherever they keep it. Revoke and reissue it as for any HTTPS site.
- **Another device on the person's tailnet:** it is the person's own device, or one they shared the
  tailnet with. It still cannot act without a paired device key; it could read the page's code, which
  holds nothing secret.
- **An agent on the PC:** it cannot produce a device signature (the key is on the phone) or add a
  device (the list is signed by the app's key). As in docs/approvals.md, an agent that tampers with
  the OS keychain to steal the app's key is out of scope.
- **A stolen phone:** revoke it in the app. Until then it can do what the person could: answer and
  approve. The phone's own lock screen is its protection; the page keeps no password.
- **A malicious page in the phone's browser:** the key lives in storage scoped to the phone server's
  origin (`http://<address>:7422` or `https://<name>:7422`), which serves only the phone page, with a strict
  Content-Security-Policy (`default-src 'self'`).
- **Denial of service** by anyone on the chosen networks: the server keeps `nitely serve`'s limits
  (connection caps, timeouts, bounded bodies).

## Why the phone signs in plain JavaScript

Browsers expose WebCrypto only to secure origins (HTTPS or localhost), and a tunnel address over
plain HTTP is not one, even though the tunnel protects it. So the page signs with a small audited
pure-JavaScript Ed25519 implementation (`@noble/ed25519` with `@noble/hashes`), bundled into the
page, with randomness from `crypto.getRandomValues`, which browsers offer on any origin. Over HTTPS
the page could use WebCrypto with a non-extractable key instead; it keeps one code path for both
until plain HTTP inside the tunnel goes away.

## The one-hand Inbox

- One decision per screen, the most urgent first, with "2 of 5" at the top and a swipe or the
  arrows to move between them.
- The issue's key, title and track (the same `StationTrack` as the desktop), and what the decision
  is: the question, or why the approval waits (the playbook's reasons) with the review's verdict and
  findings.
- The actions sit at the bottom, within a thumb's reach, as full-width buttons at least 48 px tall:
  the ask's options one per button (and "Other answer" opening a text field), or **Approve** and
  **Send back** (which asks for a reason).
- After an action the next decision slides in; when none is left the page says so and how many
  wait at the PC.
- Light and dark follow the phone. No pinch-zoom traps: text is at least 16 px so inputs do not zoom.

## Notifications

When a new question or approval waits, the app tells each paired phone that asked for it, through
that phone's own browser push service (Google's for Chrome on Android, Apple's for Safari,
Mozilla's, Microsoft's). Nitely runs no server for it.

- **On the phone:** **Notify me** in the page's header asks the phone for permission, subscribes
  its browser under the app's VAPID public key (`PushManager.subscribe`), and posts the
  subscription to the app as a signed request (`POST /phone/api/push`; an empty endpoint turns it
  off). A service worker (`/phone/sw.js`) shows each message and opens the page when it is tapped.
  Each time the page opens with permission given, it hands the app its current subscription again,
  since push services replace them now and then.
- **On the PC:** while the phone server runs, it looks for new decisions every 15 seconds. A
  decision is new the first time it is seen waiting; what already waited when the server started is
  not told. New decisions go out together, at most one message every two minutes, and one settled
  before its turn is dropped. A phone whose message did not go out (its push service was down)
  is told those decisions again with its next message, while they still wait. The message ("NIT-12 asks: Which currency?") is encrypted to the
  phone's key (RFC 8291, `aes128gcm`), so the push service carries it without reading it, and the
  app signs a VAPID token (RFC 8292, ES256) with a P-256 key it makes once and keeps in the OS
  keychain (`phone-vapid`). A subscription the push service calls gone (404, 410), or one whose
  phone was removed, is forgotten. Subscriptions are kept in `phone-push.json` in the data
  directory.
- **Only push services:** the app posts only to HTTPS endpoints of the known push services
  (`fcm.googleapis.com`, `*.push.apple.com`, `*.push.services.mozilla.com`, `*.notify.windows.com`),
  so a paired phone cannot make the PC post elsewhere.

What the person must do:

- Serve the phone page over **HTTPS** ("Choosing where it listens"): browsers offer push only to
  secure origins, and plain HTTP inside Tailscale is not one. Pair the phone from the HTTPS link.
- **iPhone:** iOS shows web push only to a page added to the Home Screen (iOS 16.4 or later), and
  that page keeps its own storage, apart from Safari's. So pair from there: scan the QR code, tap
  **Copy pairing link** on the pairing screen, then Share, Add to Home Screen, open Nitely from the
  Home Screen and paste the link. Then **Notify me**.
- **Android:** Chrome needs nothing more than **Notify me**; adding the page to the home screen is
  optional.
- Allow notifications when the phone asks. The PC must be on, with the Nitely app open and the
  phone server on, for messages to go out; the push service keeps a message for a day for a phone
  that is off.

## Away from home

The phone reaches the PC from anywhere through Tailscale, which the person runs on both: the phone
server listens on this PC's Tailscale address (or serves HTTPS under its tailnet name), and a
phone on the same tailnet reaches that address over any network it is on, at home or not. Nitely
adds nothing between them.

What Nitely does for it:

- **Listens on the tailnet interface:** the Tailscale address is offered (and picked by default)
  where the phone server listens; it is checked against what `tailscale ip` reports
  ("Choosing where it listens").
- **Puts a reachable address in the pairing QR code:** the code carries the first of the page's
  links, and those list the Tailscale address (or the HTTPS name) before this PC's loopback, which
  a phone cannot reach (`Running::pages`).
- **Says where Tailscale stands** on the Phone screen ("Away from home"): not installed, installed
  but not connected, or connected with this PC's tailnet name and address; the phones on the
  tailnet and whether Tailscale sees them online; and whether the phone server listens there
  (`tailscale status --json`, `tailscale_state`).
- **Says what to check on the phone** when the PC does not answer: Tailscale on, the PC awake with
  Nitely open and the phone server on.

What the person does: install Tailscale on the PC and the phone and sign both in with the same
account; turn the phone server on with the Tailscale address; pair; keep Tailscale on on the phone.
The PC must be awake: tick **Keep this PC awake while the phone server is on** when turning it on
(off by default, since it costs battery on a laptop). While the server runs, Nitely then asks the
OS not to sleep (`awake.rs`: `SetThreadExecutionState` on Windows, `caffeinate` on macOS,
`systemd-inhibit` on Linux); the display can still turn off, and turning the server off or
quitting the app lets go.

### A relay Nitely runs: not wanted

A relay would let a phone without Tailscale reach the PC through a server Nitely operates. It
would hold no keys (the phone signs every action, the app checks every signature), but it is not
wanted:

- It would serve the page that holds the phone's key, so whoever runs it could replace that page
  ("Choosing where it listens"): the trust the tunnel or HTTPS gives would move to Nitely's server.
- It is a service to run, pay for and keep up, for a product that runs on the person's machine.
- Tailscale's free plan, or any tunnel the person already runs, gives the same reach with
  encryption end to end, and the PC stays the only server.

Tailscale's own ways to publish a local server fit this without Nitely changing: `tailscale serve`
puts the loopback server under the tailnet name with HTTPS inside the tailnet. Tailscale Funnel
would put the pairing endpoint on the public internet, which the phone server is not built for;
it is not supported.

### What is tested, and what not

Tested here, on loopback: pairing, signed requests, decisions, HTTPS with a given certificate, the
order of the pairing links, and reading `tailscale status`. Not tested here: a real tailnet (no
Tailscale on the machine this was built on), so binding a real Tailscale address, a phone reaching
it over a cellular network, `tailscale cert`, and the Phone screen's live Tailscale state against a
real `tailscale status`.

## Build order

1. [NIT-144](nitely://issue/NIT-144): the phone server in the app, pairing, the signed device list, revoking.
2. [NIT-145](nitely://issue/NIT-145): the phone page and its decisions.
3. [NIT-146](nitely://issue/NIT-146): signed answers, approvals and send-backs, and `land` checking phone approvals.

## Code (as built)

- `crates/nitely-core/src/phone.rs`: which addresses may serve, pairing offers and requests, the
  signed device list in the keychain, signed requests, the phone server's routes.
- Away from home: `tailscale_state` and `Running::pages` in `phone.rs`; `AwayFromHome` in
  `src/PhoneSetup.tsx`; `unreachable` in `src/phone/api.ts`.
- `crates/nitely-core/src/http.rs`: the strict HTTP layer it shares with `nitely serve`, over any
  `Conn` (a TCP stream, or `phone.rs`'s TLS over one).
- HTTPS (`phone.rs`): `Tls` from PEM files (`tls_from_files`) or from `tailscale cert`
  (`tailscale_tls`), with rustls; `start` takes it, and `Running::pages` gives the `https://` link.
- `src-tauri/src/phone.rs`: turning the server on and off, pairing and revoking from the app. A
  development build (`npm run tauri dev`) embeds no frontend and serves the phone page from `dist/`
  as the last `npm run build` left it; until that has built `phone.html`, the phone gets a page
  saying to run `npm run build` and reload, with no restart of the app.
- `src/PhoneSetup.tsx`: the app's Phone screen. `phone.html`, `src/phone/`: the phone page.
- `crates/nitely-core/examples/phone_dev.rs`: the real phone server on loopback with a stand-in for
  the app's Phone screen, to pair a browser tab as the phone end to end (`npm run build`, then
  `cargo run -p nitely-core --example phone_dev`).
- Phone decisions (`/phone/api/decisions`, `approve`, `answer`, `sendback`) and the last day's
  report (`/phone/api/report`, read-only, `src/phone/Report.tsx`) are in `phone.rs`;
  `approval.rs` checks phone-signed approvals in `land` against the signed device list (the
  `approval` table's `device` and `signed_at`). The harness seeds a scratch database with decisions.
- Notifications: `crates/nitely-core/src/push.rs` (the VAPID key, RFC 8291 encryption, sending,
  `Notifier`'s throttle), `Phone::notify_tick` and `/phone/api/push` in `phone.rs`;
  `src/phone/push.ts`, `Notify.tsx` and `public/phone/` (service worker, manifest, icons) on the
  phone. Preview the switch at `/phone.html?fixtures`, and what an iPhone or a plain-HTTP page
  shows with `&env=ios` or `&env=http`.
- `src/phone/Inbox.tsx`: the one-hand Inbox (one decision per screen, the compact track with its
  words, the ask's options or the approval's reasons and review, Approve and Send back). Preview it
  at `/phone.html?fixtures` in the browser preview, or end to end with the harness.
