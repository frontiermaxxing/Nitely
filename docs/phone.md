# Deciding from your phone

Agents wait on the person: an ask they must answer, a change they must approve before it lands.
When the person is away from the PC, that wait is dead time. This lets a phone the person paired
with the app answer those asks and approve or send back those changes, and nothing else.

It builds on signed approvals (docs/approvals.md): an approval made on the phone is signed by a key
that never leaves the phone, and `land` checks that signature as it checks the app's own.

## What the phone can do, and what not

- **Can:** see the decisions waiting on the person (asks and approvals), answer an ask, approve a
  change, send a change back to the agents with a reason.
- **Cannot:** anything else. No issue edits, no filing, no settings, no rules, no code, no other
  view of the backlog. The server the phone talks to has no other routes.
- Other kinds of decision in the desktop Inbox (your own work, a stopped land, a conflict) show on
  the phone as a count of "waiting at the PC", because acting on them needs the PC.

## The pieces

1. **The phone server** runs inside the Nitely app, not in `nitely serve`. The app is the person's
   process: it already holds the approval key, and the phone's actions are the person's. It is off
   until the person turns it on in the app (Phone, beside Other machines), and it stops when the app
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
  is safe from the network in between. Other tunnels can follow once Nitely can tell they are one. This is the first version's way, and it works on the same
  Wi-Fi as well as away from home: both devices only need to be on the person's tailnet.
- **HTTPS** with a certificate the phone already trusts (a Tailscale HTTPS certificate, or a local
  certificate authority the person installs on the phone). This is how a plain LAN without a tunnel
  will work; it is the follow-up [NIT-147](nitely://issue/NIT-147).

Plain HTTP on an ordinary LAN address is refused, and the app says why. Loopback is offered for
trying it on the PC itself.

The phone server never binds `0.0.0.0`. When the person turns it on, the app lists this machine's
addresses that qualify and the person picks the ones to listen on. Only those are bound, and only
clients on those networks are answered (as `nitely serve` does). Turning it off, or quitting the
app, closes the sockets.

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

Kept in the OS keychain (service `Nitely`, account `phone-devices`), as JSON:
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
  an encrypted, authenticated tunnel (or, later, HTTPS), so they can neither read the decisions nor
  replace the page that holds the key ("Choosing where it listens").
- **Another device on the person's tailnet:** it is the person's own device, or one they shared the
  tailnet with. It still cannot act without a paired device key; it could read the page's code, which
  holds nothing secret.
- **An agent on the PC:** it cannot produce a device signature (the key is on the phone) or add a
  device (the list is signed by the app's key). As in docs/approvals.md, an agent that tampers with
  the OS keychain to steal the app's key is out of scope.
- **A stolen phone:** revoke it in the app. Until then it can do what the person could: answer and
  approve. The phone's own lock screen is its protection; the page keeps no password.
- **A malicious page in the phone's browser:** the key lives in storage scoped to the phone server's
  origin (`http://<address>:7422`), which serves only the phone page, with a strict
  Content-Security-Policy (`default-src 'self'`).
- **Denial of service** by anyone on the chosen networks: the server keeps `nitely serve`'s limits
  (connection caps, timeouts, bounded bodies).

## Why the phone signs in plain JavaScript

Browsers expose WebCrypto only to secure origins (HTTPS or localhost), and a tunnel address over
plain HTTP is not one, even though the tunnel protects it. So the page signs with a small audited
pure-JavaScript Ed25519 implementation (`@noble/ed25519` with `@noble/hashes`), bundled into the
page, with randomness from `crypto.getRandomValues`, which browsers offer on any origin. With HTTPS
([NIT-147](nitely://issue/NIT-147)) the page can move to WebCrypto with a non-extractable key.

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

## Later

- **A plain LAN without a tunnel:** HTTPS with a certificate the phone trusts, then web push
  ([NIT-147](nitely://issue/NIT-147)).
- **Away from home:** a tunnel already reaches anywhere the phone has a connection; checking it end
  to end and the setup words on the Phone screen are [NIT-148](nitely://issue/NIT-148). A relay
  Nitely would run is out of scope.
- **Notifications on the phone:** web push needs a secure origin and a push service, so it waits for
  HTTPS ([NIT-147](nitely://issue/NIT-147)). Until then, the desktop notifications (NIT-124) and
  a home-screen shortcut to the phone page.

## Build order

1. [NIT-144](nitely://issue/NIT-144): the phone server in the app, pairing, the signed device list, revoking.
2. [NIT-145](nitely://issue/NIT-145): the phone page and its decisions.
3. [NIT-146](nitely://issue/NIT-146): signed answers, approvals and send-backs, and `land` checking phone approvals.

## Code (as built)

- `crates/nitely-core/src/phone.rs`: which addresses may serve, pairing offers and requests, the
  signed device list in the keychain, signed requests, the phone server's routes.
- `crates/nitely-core/src/http.rs`: the strict HTTP layer it shares with `nitely serve`.
- `src-tauri/src/phone.rs`: turning the server on and off, pairing and revoking from the app.
- `src/PhoneSetup.tsx`: the app's Phone screen. `phone.html`, `src/phone/`: the phone page.
- `crates/nitely-core/examples/phone_dev.rs`: the real phone server on loopback with a stand-in for
  the app's Phone screen, to pair a browser tab as the phone end to end (`npm run build`, then
  `cargo run -p nitely-core --example phone_dev`).
- Still to come: the decisions ([NIT-145](nitely://issue/NIT-145)), and phone approvals checked
  at land ([NIT-146](nitely://issue/NIT-146)).
