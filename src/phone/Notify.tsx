import { useEffect, useMemo, useState } from "react";
import type { Device } from "./keys";
import { current, notifyState, readEnv, turnOff, turnOn } from "./push";

/** The header's notification switch, and what it takes when this phone cannot have them yet. */
export function Notify(props: { device: Device }) {
  const { device } = props;
  const env = useMemo(readEnv, []);
  const state = notifyState(env);
  const [on, setOn] = useState(false);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    current(device, env).then(
      (v) => live && setOn(v),
      () => live && setOn(false),
    );
    return () => {
      live = false;
    };
  }, [device, env]);

  const run = async (what: () => Promise<string | null>, next: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const why = await what();
      if (why) setError(why);
      else {
        setOn(next);
        setOpen(false);
      }
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  return (
    <>
      <button className={`ph-bell${on ? " ph-bell-on" : ""}`} aria-expanded={open} aria-controls="ph-notify" onClick={() => setOpen(!open)}>
        <span className="ph-bell-dot" aria-hidden="true" />
        {on ? "Notifying" : "Notify me"}
      </button>
      {open && (
        <div className="ph-notify" id="ph-notify" role="region" aria-label="Notifications">
          {on ? (
            <p>This phone gets a notification when a question or approval waits for you, at most one every two minutes.</p>
          ) : (
            <p>Get a notification on this phone when a question or approval waits for you. The PC sends it through the phone's own push service, encrypted so only this phone can read it.</p>
          )}
          {state.kind === "blocked" && !on && <p className="ph-quiet">{state.why}</p>}
          {error && <p className="ph-error">{error}</p>}
          <div className="ph-notify-actions">
            {on ? (
              <button className="ph-btn" disabled={busy} onClick={() => run(() => turnOff(device), false)}>
                Turn off
              </button>
            ) : (
              <button className="ph-btn ph-primary" disabled={busy || state.kind !== "ready"} onClick={() => run(() => turnOn(device), true)}>
                {busy ? "Turning on…" : "Turn on"}
              </button>
            )}
            <button className="ph-btn" onClick={() => setOpen(false)}>
              Close
            </button>
          </div>
        </div>
      )}
    </>
  );
}
