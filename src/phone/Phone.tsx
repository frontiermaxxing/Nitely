import { useEffect, useState } from "react";
import { post, signedGet } from "./api";
import { Inbox } from "./Inbox";
import { loadDevice, newKey, pairingCode, pairMessage, saveDevice, sign, type Device } from "./keys";
import { Notify } from "./Notify";
import { readEnv } from "./push";

/** The pairing secret in a pairing link (or the page's own address, opened from the QR code). */
function secretIn(link: string): string | null {
  const m = /[#&]pair=([0-9a-f]{64})/i.exec(link);
  return m ? m[1].toLowerCase() : null;
}

function pairSecret(): string | null {
  return secretIn(location.hash);
}

/** Takes the secret out of the address bar, so it is not left in the phone's history. */
function forgetSecret() {
  history.replaceState(null, "", location.pathname + location.search);
}

function guessName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  const android = /Android[^;]*;\s*([^;)]+)/.exec(ua);
  if (android) return android[1].replace(/Build.*/, "").trim().slice(0, 40) || "Android phone";
  return "Phone";
}

type View =
  | { kind: "start" }
  | { kind: "pair"; secret: string }
  | { kind: "confirm"; device: Device }
  | { kind: "paired"; device: Device }
  | { kind: "gone"; why: string };

function initial(): View {
  const device = loadDevice();
  const secret = pairSecret();
  // A new pairing link wins: the phone may have been removed on the PC and is pairing again.
  if (secret) return { kind: "pair", secret };
  if (device?.paired) return { kind: "paired", device };
  if (device) return { kind: "confirm", device };
  return { kind: "start" };
}

/** The phone page: pairing first, then the decisions waiting on the person, one per screen. */
export function Phone() {
  const [view, setView] = useState<View>(initial);
  return (
    <main className="ph">
      <header className="ph-head">
        <span className="ph-mark" aria-hidden="true">
          ●●○
        </span>
        <span className="ph-brand">Nitely</span>
        {view.kind === "paired" && <span className="ph-who">{view.device.name}</span>}
        {view.kind === "paired" && <Notify device={view.device} />}
      </header>
      {view.kind === "start" && <Start onSecret={(secret) => setView({ kind: "pair", secret })} />}
      {view.kind === "pair" && <Pair secret={view.secret} onAsked={(device) => setView({ kind: "confirm", device })} />}
      {view.kind === "confirm" && (
        <Confirm device={view.device} onPaired={(device) => setView({ kind: "paired", device })} onGone={(why) => setView({ kind: "gone", why })} />
      )}
      {view.kind === "paired" && (
        <Inbox
          device={view.device}
          onGone={(why) => {
            saveDevice(null);
            setView({ kind: "gone", why });
          }}
        />
      )}
      {view.kind === "gone" && <Gone why={view.why} />}
    </main>
  );
}

function Start(props: { onSecret: (secret: string) => void }) {
  const [link, setLink] = useState("");
  const secret = secretIn(link);
  return (
    <section className="ph-body">
      <h1>Pair this phone</h1>
      <p>
        On the PC, open Nitely, go to <strong>Phone</strong> and press <strong>Pair a phone</strong>. Then scan the code it shows with this phone's camera.
      </p>
      <label className="ph-field">
        <span>Or paste the pairing link</span>
        <input value={link} inputMode="url" autoComplete="off" autoCapitalize="off" spellCheck={false} onChange={(e) => setLink(e.target.value)} />
      </label>
      {link.trim() && !secret && <p className="ph-error">That is not a pairing link: it ends in #pair= and 64 letters and digits.</p>}
      <div className="ph-actions">
        <button className="ph-btn ph-primary" disabled={!secret} onClick={() => secret && props.onSecret(secret)}>
          Continue
        </button>
      </div>
    </section>
  );
}

/**
 * On an iPhone's browser, before pairing: notifications come only to the page on the Home Screen,
 * which keeps its own storage, so the key must be made there. Copying the link lets the person
 * pair from there instead.
 */
function HomeScreenHint() {
  const env = readEnv();
  const [copied, setCopied] = useState(false);
  if (!env.ios || env.standalone || !env.secure) return null;
  return (
    <div className="ph-hint">
      <p>
        <strong>Want notifications?</strong> On iPhone they come only to Nitely on the Home Screen, which pairs on its own. Copy this pairing link, tap Share, then Add to
        Home Screen, open Nitely from there and paste the link.
      </p>
      <button
        className="ph-btn"
        onClick={() =>
          navigator.clipboard.writeText(location.href).then(
            () => setCopied(true),
            () => setCopied(false),
          )
        }
      >
        {copied ? "Copied" : "Copy pairing link"}
      </button>
    </div>
  );
}

function Pair(props: { secret: string; onAsked: (d: Device) => void }) {
  const [name, setName] = useState(guessName);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pair = async () => {
    setBusy(true);
    setError(null);
    try {
      const key = newKey();
      const code = pairingCode(props.secret, key.public_key);
      const r = await post<{ id: string; code: string }>("/phone/api/pair", {
        secret: props.secret,
        name: name.trim(),
        public_key: key.public_key,
        signature: sign(key.secret, pairMessage(props.secret, key.public_key)),
      });
      forgetSecret();
      if (r.status !== 200) {
        setError(r.body.error ?? `The PC answered ${r.status}.`);
        setBusy(false);
        return;
      }
      if (r.body.code !== code) {
        // The PC worked out another code than this phone did: something is not what it seems.
        setError("The PC's pairing code does not match this phone's. Decline it in the Nitely app and pair again.");
        setBusy(false);
        return;
      }
      const device: Device = { id: r.body.id, name: name.trim(), secret: key.secret, public_key: key.public_key, code, paired: false };
      saveDevice(device);
      props.onAsked(device);
    } catch (e) {
      setError(`Could not reach the PC: ${String(e)}`);
      setBusy(false);
    }
  };

  return (
    <section className="ph-body">
      <h1>Pair this phone</h1>
      <p>This phone will be able to answer the agents' questions and approve their changes. Nothing else.</p>
      <HomeScreenHint />
      <label className="ph-field">
        <span>Name it</span>
        <input value={name} maxLength={40} autoComplete="off" onChange={(e) => setName(e.target.value)} />
      </label>
      {error && <p className="ph-error">{error}</p>}
      <div className="ph-actions">
        <button className="ph-btn ph-primary" disabled={busy || !name.trim()} onClick={pair}>
          {busy ? "Pairing…" : "Pair"}
        </button>
      </div>
    </section>
  );
}

function Confirm(props: { device: Device; onPaired: (d: Device) => void; onGone: (why: string) => void }) {
  const { device } = props;
  const [problem, setProblem] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    const poll = async () => {
      if (!live) return;
      try {
        const r = await signedGet<{ status: string }>(device, "/phone/api/pair/status");
        if (!live) return;
        if (r.status === 200 && r.body.status === "paired") {
          const paired = { ...device, paired: true };
          saveDevice(paired);
          return props.onPaired(paired);
        }
        if (r.status === 404 || (r.status === 200 && r.body.status === "declined")) {
          saveDevice(null);
          return props.onGone("The pairing was declined in the Nitely app, or it expired.");
        }
        // A clock that is off, say: tell the person while still asking.
        setProblem(r.status === 200 ? null : (r.body.error ?? `The PC answered ${r.status}.`));
      } catch {
        // The PC is out of reach for a moment; keep asking.
      }
      setTimeout(poll, 2000);
    };
    poll();
    return () => {
      live = false;
    };
  }, [device]);

  return (
    <section className="ph-body">
      <h1>Confirm on the PC</h1>
      <p>The Nitely app asks whether to pair “{device.name}”. Check it shows this code, then press Pair there.</p>
      <p className="ph-code" aria-label={`Pairing code ${device.code.split("").join(" ")}`}>
        {device.code}
      </p>
      <p className="ph-quiet">Waiting for the PC…</p>
      {problem && <p className="ph-error">{problem}</p>}
      <div className="ph-actions">
        <button
          className="ph-btn"
          onClick={() => {
            saveDevice(null);
            props.onGone("Pairing canceled on this phone.");
          }}
        >
          Cancel
        </button>
      </div>
    </section>
  );
}

function Gone(props: { why: string }) {
  return (
    <section className="ph-body">
      <h1>Not paired</h1>
      <p>{props.why}</p>
      <p>To pair again, press Pair a phone in the Nitely app on the PC and scan the new code.</p>
    </section>
  );
}
