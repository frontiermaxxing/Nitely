import { useCallback, useEffect, useState } from "react";
import { invoke } from "./backend";
import type { PhoneOffer, PhoneStatus, TailscaleState } from "./types";
import { Button, Checkbox, Loading, Notice, RadioCards, RemoveButton, SettingsRow, TextField, type ChoiceOption } from "./ui";
import { ago } from "./util";
import "./phone-setup.css";

/**
 * Pairing a phone and the paired phones (docs/phone.md). The phone server runs in this app, off
 * until the person turns it on, and only where nobody in between can change the page: inside
 * Tailscale, or over HTTPS with a certificate the phone trusts. A phone pairs only when the person
 * confirms it here, after comparing the code on both screens.
 */
export function PhoneSetup(props: { now: number }) {
  const [status, setStatus] = useState<PhoneStatus | null>(null);
  const [picked, setPicked] = useState<string[]>([]);
  const [offer, setOffer] = useState<PhoneOffer | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // How the phone reaches the page: plain HTTP inside Tailscale, or HTTPS with a certificate the
  // phone trusts (Tailscale's for this PC, or the person's own).
  const [mode, setMode] = useState<"none" | "tailscale" | "files">("none");
  const [tsName, setTsName] = useState<{ name: string | null; why: string | null } | null>(null);
  const [files, setFiles] = useState({ cert: "", key: "", host: "" });
  const [keepAwake, setKeepAwake] = useState(false);
  useEffect(() => {
    if (mode !== "tailscale" || tsName) return;
    invoke<string>("phone_tailscale_name").then(
      (name) => setTsName({ name, why: null }),
      (e) => setTsName({ name: null, why: String(e) }),
    );
  }, [mode, tsName]);

  const load = useCallback(() => {
    invoke<PhoneStatus>("phone_status").then(
      (s) => {
        setStatus(s);
        setPicked((p) => (p.length ? p : s.addresses.filter((a) => a.kind === "tunnel").map((a) => a.ip)));
      },
      (e) => setError(String(e)),
    );
  }, []);
  useEffect(load, [load]);
  // While the server is on, a phone's pairing request can arrive any moment, code shown or hidden.
  const serving = (status?.listening.length ?? 0) > 0;
  useEffect(() => {
    if (!serving) return;
    const t = setInterval(load, 2000);
    return () => clearInterval(t);
  }, [serving, load]);

  const run = async (work: () => Promise<PhoneStatus | void>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await work();
      if (s) setStatus(s);
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  if (!status) return error ? <p className="error setting-feedback">{error}</p> : <Loading />;
  const on = status.listening.length > 0;
  const tunnels = status.addresses.filter((a) => a.kind === "tunnel");
  const https = mode !== "none";
  // Over HTTPS the certificate guards the page, so any of this PC's addresses will do.
  const usable = (a: (typeof status.addresses)[number]) => https || !!a.kind;
  const ready = mode === "none" || (mode === "tailscale" ? !!tsName?.name : !!(files.cert.trim() && files.key.trim() && files.host.trim()));
  const tls = mode === "none" ? null : { kind: mode, ...files };
  const expired = offer !== null && offer.expires < props.now;

  const modes: ChoiceOption<typeof mode>[] = [
    { value: "none", label: "Inside Tailscale", why: "Plain HTTP on this PC's Tailscale address; Tailscale on the phone too." },
    {
      value: "tailscale",
      label: "HTTPS with Tailscale's certificate",
      why: (
        <>
          A real certificate for this PC's tailnet name, which phones trust. Turn on MagicDNS and HTTPS Certificates in the Tailscale admin console first.
          {mode === "tailscale" && tsName && <span className={`phone-ts-name ${tsName.name ? "mono" : "error"}`}>{tsName.name ?? tsName.why}</span>}
        </>
      ),
    },
    { value: "files", label: "HTTPS with your own certificate", why: "PEM files, for a name the phone resolves to this PC, from a certificate authority the phone trusts." },
  ];

  return (
    <div className="phone-setup">
      {error && <p className="error setting-feedback">{error}</p>}

      <div className="setting-sub">
        <h3>Where it listens</h3>
        {on ? (
          <>
            <SettingsRow label="The phone page" why={status.awake ? "This PC stays awake while the phone server is on." : undefined}>
              <ul className="setting-value mono phone-urls">
                {status.listening.map((u) => (
                  <li key={u}>{u}</li>
                ))}
              </ul>
            </SettingsRow>
            <div className="setting-actions">
              <Button
                disabled={busy}
                onClick={() =>
                  run(async () => {
                    setOffer(null);
                    return invoke<PhoneStatus>("phone_stop");
                  })
                }
              >
                Turn off
              </Button>
            </div>
          </>
        ) : (
          <>
            <SettingsRow
              label="How the phone reaches it"
              why="The phone page holds the phone's key, so nobody between the PC and the phone may be able to change it: the page goes only inside Tailscale, or over HTTPS with a certificate the phone trusts."
            >
              <RadioCards
                options={modes}
                value={mode}
                onChange={(m) => {
                  if (m === "tailscale") setTsName(null);
                  setMode(m);
                }}
                aria-label="How the phone reaches it"
              />
              {mode === "files" && (
                <div className="phone-files">
                  <TextField mono placeholder="Certificate file (PEM)" aria-label="Certificate file" value={files.cert} onChange={(e) => setFiles({ ...files, cert: e.target.value })} />
                  <TextField mono placeholder="Private key file (PEM)" aria-label="Private key file" value={files.key} onChange={(e) => setFiles({ ...files, key: e.target.value })} />
                  <TextField mono placeholder="Name on the certificate" aria-label="Name on the certificate" value={files.host} onChange={(e) => setFiles({ ...files, host: e.target.value })} />
                </div>
              )}
            </SettingsRow>
            <SettingsRow
              label="Addresses"
              why={tunnels.length === 0 && mode !== "files" ? "No Tailscale address yet. Install Tailscale on this PC and on the phone, sign both in, and it shows up here." : "Where the phone page listens."}
            >
              <div className="setting-list">
                {status.addresses.map((a) => (
                  <div key={a.ip} className="setting-list-item">
                    <Checkbox
                      disabled={!usable(a)}
                      checked={usable(a) && picked.includes(a.ip)}
                      onChange={(on) => setPicked(on ? [...picked, a.ip] : picked.filter((p) => p !== a.ip))}
                      label={<span className="mono">{a.ip}</span>}
                    />
                    <span className="setting-unit">
                      {a.interface}
                      {a.kind === "loopback" ? ", for trying it on this PC" : a.kind === "tunnel" ? ", tunnel" : https ? "" : ", ordinary network: HTTPS only"}
                    </span>
                  </div>
                ))}
              </div>
            </SettingsRow>
            <SettingsRow label="Keep this PC awake" why="So your phone finds it away from home. The screen can still turn off; on a laptop it costs battery.">
              <span className="setting-inline">
                <Checkbox checked={keepAwake} onChange={setKeepAwake} label="While the phone server is on" />
              </span>
            </SettingsRow>
            <div className="setting-actions">
              <Button
                variant="primary"
                disabled={busy || !ready || !status.addresses.some((a) => usable(a) && picked.includes(a.ip))}
                onClick={() => run(() => invoke<PhoneStatus>("phone_start", { ips: picked.filter((ip) => status.addresses.some((a) => a.ip === ip && usable(a))), tls, keepAwake }))}
              >
                Turn on
              </Button>
            </div>
          </>
        )}
      </div>

      {on && (
        <div className="setting-sub">
          <h3>Pair a phone</h3>
          {status.requests.map((r) => (
            <div className="setting-notice" key={r.id}>
              <Notice tone="warning" lead={`Pair “${r.name}”?`}>
                Only if the phone shows <span className="phone-code">{r.code}</span>
              </Notice>
              <div className="setting-actions">
                <Button
                  variant="primary"
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      setOffer(null);
                      return invoke<PhoneStatus>("phone_decide", { id: r.id, accept: true });
                    })
                  }
                >
                  Pair
                </Button>
                <Button disabled={busy} onClick={() => run(() => invoke<PhoneStatus>("phone_decide", { id: r.id, accept: false }))}>
                  Decline
                </Button>
              </div>
            </div>
          ))}
          {offer && !expired ? (
            <div className="phone-offer">
              {/* The SVG is the app's own QR code of its own link. */}
              <div className="phone-qr" dangerouslySetInnerHTML={{ __html: offer.qr_svg }} aria-label="Pairing code to scan with the phone" role="img" />
              <div className="phone-offer-text small">
                <p>Scan this with the phone's camera. It works once, for ten minutes, and the phone pairs only when you confirm it here.</p>
                <p className="mono phone-link">{offer.urls[0]}</p>
                <Button variant="quiet" onClick={() => setOffer(null)}>
                  Hide the code
                </Button>
              </div>
            </div>
          ) : (
            <div className="setting-actions">
              <Button variant="primary" disabled={busy} onClick={() => run(async () => setOffer(await invoke<PhoneOffer>("phone_pair")))}>
                {expired ? "Make a new code" : "Pair a phone"}
              </Button>
            </div>
          )}
          <p className="setting-note">
            {status.listening.some((u) => u.startsWith("https://"))
              ? "For notifications, tap Notify me on the phone once it is paired. On iPhone they come only to the page added to the Home Screen: add it there first and pair from there."
              : "Notifications need HTTPS: turn the phone server off and on with HTTPS chosen, then pair the phone from the new link."}
          </p>
        </div>
      )}

      <div className="setting-sub">
        <h3>Paired phones</h3>
        {status.devices_error && <p className="error setting-feedback">{status.devices_error}</p>}
        {status.devices.length === 0 && !status.devices_error && <p className="setting-note">None yet.</p>}
        <div className="phone-devices">
          {status.devices.map((d) => (
            <div className="setting-item" key={d.id}>
              <span className="setting-item-main">
                <span>{d.name}</span>
                <span className="setting-unit">
                  paired {ago(props.now, d.paired_at)}
                  {status.notified.includes(d.id) && " · notifications on"}
                </span>
              </span>
              {removing === d.id ? (
                <span className="setting-confirm">
                  Its approvals not landed yet stop counting.
                  <Button variant="danger" disabled={busy} onClick={() => run(async () => (setRemoving(null), invoke<PhoneStatus>("phone_revoke", { id: d.id })))}>
                    Remove
                  </Button>
                  <Button variant="quiet" onClick={() => setRemoving(null)}>
                    Keep
                  </Button>
                </span>
              ) : (
                <RemoveButton label={`Remove ${d.name}`} onClick={() => setRemoving(d.id)} />
              )}
            </div>
          ))}
        </div>
      </div>

      <AwayFromHome bound={status.bound} />
    </div>
  );
}

/**
 * Reaching the phone server from anywhere: through Tailscale, which the person runs on this PC and
 * the phone. Nitely runs no relay (docs/phone.md, "Away from home"); this says where Tailscale
 * stands and what is left to do.
 */
function AwayFromHome(props: { bound: string[] }) {
  const [ts, setTs] = useState<TailscaleState | null>(null);
  const [checking, setChecking] = useState(false);
  const check = useCallback(() => {
    setChecking(true);
    invoke<TailscaleState>("phone_tailscale").then(
      (s) => (setTs(s), setChecking(false)),
      () => (setTs(null), setChecking(false)),
    );
  }, []);
  useEffect(check, [check]);
  // Whether the server is bound to one of Tailscale's addresses for this PC, over HTTP or HTTPS.
  const viaTailscale = !!ts && props.bound.some((ip) => ts.ips.includes(ip));

  return (
    <div className="setting-sub phone-away">
      <h3>Away from home</h3>
      <p className="setting-note">
        A paired phone reaches this PC from anywhere it has internet through Tailscale, a private network you run on your own devices. Nitely runs no server in between.
      </p>
      {ts && (
        <p className="setting-note phone-away-state" role="status">
          {!ts.installed
            ? "Tailscale is not on this PC yet."
            : !ts.running
              ? `Tailscale is on this PC but not connected (${ts.backend || "stopped"}): open Tailscale and sign in.`
              : `Tailscale is connected: this PC is ${ts.name ?? ts.ips[0] ?? "on your tailnet"}${ts.ips[0] ? ` (${ts.ips[0]})` : ""}.`}
          {ts.running &&
            (ts.phones.length
              ? ` Phones on your tailnet: ${ts.phones.map((p) => `${p.name} (${p.online ? "online" : "offline"})`).join(", ")}.`
              : " No phone on your tailnet yet.")}
          {ts.running && props.bound.length > 0 && (viaTailscale ? " The phone server listens there." : " The phone server does not listen on the Tailscale address: turn it off and on with that address picked.")}
        </p>
      )}
      <ol className="setting-note phone-away-steps">
        <li>Install Tailscale on this PC and on the phone, and sign both in with the same account.</li>
        <li>Turn the phone server on with this PC's Tailscale address, or with HTTPS and Tailscale's certificate.</li>
        <li>Pair the phone: the code it scans carries that address.</li>
        <li>Away from home, keep Tailscale on on the phone. This PC must be awake, with Nitely open: tick "Keep this PC awake" when you turn the phone server on.</li>
      </ol>
      <div className="setting-actions">
        <Button variant="quiet" disabled={checking} onClick={check}>
          {checking ? "Checking…" : "Check again"}
        </Button>
      </div>
    </div>
  );
}
