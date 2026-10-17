import { useCallback, useEffect, useState } from "react";
import { invoke } from "./backend";
import type { PhoneOffer, PhoneStatus } from "./types";
import { Loading } from "./ui";
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

  if (!status) return <div className="page">{error ? <p className="error">{error}</p> : <Loading />}</div>;
  const on = status.listening.length > 0;
  const tunnels = status.addresses.filter((a) => a.kind === "tunnel");
  const https = mode !== "none";
  // Over HTTPS the certificate guards the page, so any of this PC's addresses will do.
  const usable = (a: (typeof status.addresses)[number]) => https || !!a.kind;
  const ready = mode === "none" || (mode === "tailscale" ? !!tsName?.name : !!(files.cert.trim() && files.key.trim() && files.host.trim()));
  const tls = mode === "none" ? null : { kind: mode, ...files };
  const expired = offer !== null && offer.expires < props.now;

  return (
    <div className="page page-form phone-setup">
      <div className="page-head">
        <h1>Phone</h1>
      </div>
      <p className="page-lead">
        Answer the agents' questions and approve their changes from your phone. A paired phone can do that and nothing else, and every approval it gives is
        signed with a key that never leaves it.
      </p>
      {error && <p className="error">{error}</p>}

      <section className="card">
        <div className="card-title">Where it listens</div>
        {on ? (
          <>
            <p className="small">The phone page is on:</p>
            <ul className="mono small phone-urls">
              {status.listening.map((u) => (
                <li key={u}>{u}</li>
              ))}
            </ul>
            <div className="actions">
              <button
                className="btn"
                disabled={busy}
                onClick={() =>
                  run(async () => {
                    setOffer(null);
                    return invoke<PhoneStatus>("phone_stop");
                  })
                }
              >
                Turn off
              </button>
            </div>
          </>
        ) : (
          <>
            <p className="small muted">
              The phone page holds the phone's key, so nobody between the PC and the phone may be able to change it: the page goes only inside Tailscale, or over
              HTTPS with a certificate the phone trusts.
            </p>
            <div className="phone-modes" role="radiogroup" aria-label="How the phone reaches it">
              <label className="phone-mode">
                <input type="radio" name="phone-mode" checked={mode === "none"} onChange={() => setMode("none")} />
                <span>
                  <strong>Inside Tailscale</strong>
                  <span className="small muted"> Plain HTTP on this PC's Tailscale address; Tailscale on the phone too.</span>
                </span>
              </label>
              <label className="phone-mode">
                <input type="radio" name="phone-mode" checked={mode === "tailscale"} onChange={() => { setTsName(null); setMode("tailscale"); }} />
                <span>
                  <strong>HTTPS with Tailscale's certificate</strong>
                  <span className="small muted"> A real certificate for this PC's tailnet name, which phones trust. Turn on MagicDNS and HTTPS Certificates in the Tailscale admin console first.</span>
                  {mode === "tailscale" && tsName && (
                    <span className={`small ${tsName.name ? "mono" : "error"}`}>{tsName.name ?? tsName.why}</span>
                  )}
                </span>
              </label>
              <label className="phone-mode">
                <input type="radio" name="phone-mode" checked={mode === "files"} onChange={() => setMode("files")} />
                <span>
                  <strong>HTTPS with your own certificate</strong>
                  <span className="small muted"> PEM files, for a name the phone resolves to this PC, from a certificate authority the phone trusts.</span>
                </span>
              </label>
              {mode === "files" && (
                <div className="phone-files">
                  <input className="mono" placeholder="Certificate file (PEM)" aria-label="Certificate file" value={files.cert} onChange={(e) => setFiles({ ...files, cert: e.target.value })} />
                  <input className="mono" placeholder="Private key file (PEM)" aria-label="Private key file" value={files.key} onChange={(e) => setFiles({ ...files, key: e.target.value })} />
                  <input className="mono" placeholder="Name on the certificate" aria-label="Name on the certificate" value={files.host} onChange={(e) => setFiles({ ...files, host: e.target.value })} />
                </div>
              )}
            </div>
            <div className="phone-addresses">
              {status.addresses.map((a) => (
                <label key={a.ip} className={`phone-address${usable(a) ? "" : " phone-address-no"}`}>
                  <input
                    type="checkbox"
                    disabled={!usable(a)}
                    checked={usable(a) && picked.includes(a.ip)}
                    onChange={(e) => setPicked(e.target.checked ? [...picked, a.ip] : picked.filter((p) => p !== a.ip))}
                  />
                  <span className="mono">{a.ip}</span>
                  <span className="small muted">
                    {a.interface}
                    {a.kind === "loopback" ? ", for trying it on this PC" : a.kind === "tunnel" ? ", tunnel" : https ? "" : ", ordinary network: HTTPS only"}
                  </span>
                </label>
              ))}
            </div>
            {tunnels.length === 0 && mode !== "files" && <p className="small">No Tailscale address yet. Install Tailscale on this PC and on the phone, sign both in, and it shows up here.</p>}
            <div className="actions">
              <button
                className="btn primary"
                disabled={busy || !ready || !status.addresses.some((a) => usable(a) && picked.includes(a.ip))}
                onClick={() => run(() => invoke<PhoneStatus>("phone_start", { ips: picked.filter((ip) => status.addresses.some((a) => a.ip === ip && usable(a))), tls }))}
              >
                Turn on
              </button>
            </div>
          </>
        )}
      </section>

      {on && (
        <section className="card">
          <div className="card-title">Pair a phone</div>
          {status.requests.map((r) => (
            <div className="phone-request" key={r.id} role="alert">
              <p>
                Pair <strong>“{r.name}”</strong>? Only if the phone shows <span className="phone-code">{r.code}</span>
              </p>
              <div className="actions">
                <button
                  className="btn primary"
                  disabled={busy}
                  onClick={() =>
                    run(async () => {
                      setOffer(null);
                      return invoke<PhoneStatus>("phone_decide", { id: r.id, accept: true });
                    })
                  }
                >
                  Pair
                </button>
                <button className="btn" disabled={busy} onClick={() => run(() => invoke<PhoneStatus>("phone_decide", { id: r.id, accept: false }))}>
                  Decline
                </button>
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
                <button className="btn quiet" onClick={() => setOffer(null)}>
                  Hide the code
                </button>
              </div>
            </div>
          ) : (
            <div className="actions">
              <button className="btn primary" disabled={busy} onClick={() => run(async () => setOffer(await invoke<PhoneOffer>("phone_pair")))}>
                {expired ? "Make a new code" : "Pair a phone"}
              </button>
            </div>
          )}
        </section>
      )}

      <section className="card">
        <div className="card-title">Paired phones</div>
        {status.devices_error && <p className="error small">{status.devices_error}</p>}
        {status.devices.length === 0 && !status.devices_error && <p className="small muted">None yet.</p>}
        <div className="phone-devices">
          {status.devices.map((d) => (
            <div className="phone-device" key={d.id}>
              <span>{d.name}</span>
              <span className="small muted">paired {ago(props.now, d.paired_at)}</span>
              {removing === d.id ? (
                <span className="phone-confirm small">
                  Its approvals not landed yet stop counting.
                  <button className="btn danger" disabled={busy} onClick={() => run(async () => (setRemoving(null), invoke<PhoneStatus>("phone_revoke", { id: d.id })))}>
                    Remove
                  </button>
                  <button className="btn quiet" onClick={() => setRemoving(null)}>
                    Keep
                  </button>
                </span>
              ) : (
                <button className="btn quiet" onClick={() => setRemoving(d.id)}>
                  Remove
                </button>
              )}
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}
