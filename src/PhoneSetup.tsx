import { useCallback, useEffect, useState } from "react";
import { invoke } from "./backend";
import type { PhoneOffer, PhoneStatus } from "./types";
import { Loading } from "./ui";
import { ago } from "./util";
import "./phone-setup.css";

/**
 * Pairing a phone and the paired phones (docs/phone.md). The phone server runs in this app, off
 * until the person turns it on, and only on addresses whose path to the phone nobody in between
 * can change: this machine, or a tunnel such as Tailscale. A phone pairs only when the person
 * confirms it here, after comparing the code on both screens.
 */
export function PhoneSetup(props: { now: number }) {
  const [status, setStatus] = useState<PhoneStatus | null>(null);
  const [picked, setPicked] = useState<string[]>([]);
  const [offer, setOffer] = useState<PhoneOffer | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
              Only addresses whose path to the phone is encrypted end to end: this PC's Tailscale address, with Tailscale on the phone too. Plain Wi-Fi is not offered,
              because anyone who can intercept it could change the page that holds the phone's key.
            </p>
            <div className="phone-addresses">
              {status.addresses.map((a) => (
                <label key={a.ip} className={`phone-address${a.kind ? "" : " phone-address-no"}`}>
                  <input
                    type="checkbox"
                    disabled={!a.kind}
                    checked={picked.includes(a.ip)}
                    onChange={(e) => setPicked(e.target.checked ? [...picked, a.ip] : picked.filter((p) => p !== a.ip))}
                  />
                  <span className="mono">{a.ip}</span>
                  <span className="small muted">
                    {a.interface}
                    {a.kind === "loopback" ? ", for trying it on this PC" : a.kind === "tunnel" ? ", tunnel" : ", ordinary network: not offered"}
                  </span>
                </label>
              ))}
            </div>
            {tunnels.length === 0 && <p className="small">No tunnel address yet. Install Tailscale on this PC and on the phone, sign both in, and it shows up here.</p>}
            <div className="actions">
              <button className="btn primary" disabled={busy || picked.length === 0} onClick={() => run(() => invoke<PhoneStatus>("phone_start", { ips: picked }))}>
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
