import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { SettingsRow, Switch } from "./ui";

/** The listener for issue links at login (crates/nitely-core/src/link_service.rs, NIT-229). */
export interface LinksStatus {
  /** False under the test harness or a stand-in agents' home: nothing is registered there. */
  allowed: boolean;
  /** You turned it off. */
  off: boolean;
  registered: boolean;
  /** Registered with this Nitely's own command. */
  current: boolean;
  /** Who answers links on the port now. */
  answering: "app" | "service" | null;
  place: string;
  command: string;
  port: number;
}

/**
 * Settings' Issue links section: whether the small listener that starts at login is on, and who
 * answers links now. Off, links still open while the app runs.
 */
export function LinksSettings(props: { version: number }) {
  const [status, setStatus] = useState<LinksStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    invoke<LinksStatus>("links_service").then(
      (s) => live && (setStatus(s), setError(null)),
      (e) => live && setError(String(e)),
    );
    return () => {
      live = false;
    };
  }, [props.version]);

  const set = (on: boolean) => {
    setBusy(true);
    invoke<LinksStatus>("set_links_service", { on })
      .then((s) => (setStatus(s), setError(null)))
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  if (error && !status) return <p className="error setting-feedback">{error}</p>;
  if (!status) return null;
  const on = status.registered && !status.off;
  const example = `http://127.0.0.1:${status.port}/i/NIT-12`;
  const why = !status.allowed
    ? "This copy of Nitely never registers anything at login (test harness)."
    : on
      ? status.current
        ? "A small listener starts at login, with no window. A link opens the issue in Nitely, starting the app first when it is closed."
        : "Registered with another copy of Nitely; turn it off and on to use this one."
      : "Off: links open only while Nitely runs.";
  return (
    <>
      <SettingsRow label="Open issue links even when Nitely is closed" why={why}>
        <Switch checked={on} disabled={busy || !status.allowed} onChange={set} aria-label="Open issue links even when Nitely is closed" />
      </SettingsRow>
      <SettingsRow label="Answering now" why={<>Links look like <span className="mono">{example}</span>.</>}>
        <span className="setting-value">
          {status.answering === "service" ? "The listener" : status.answering === "app" ? "This app" : "Nothing"}
          {on && (
            <>
              {", registered at "}
              <span className="mono path">{status.place}</span>
            </>
          )}
        </span>
      </SettingsRow>
      {error && <p className="error setting-feedback">{error}</p>}
    </>
  );
}
