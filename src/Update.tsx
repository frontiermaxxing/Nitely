import { useEffect, useState } from "react";
import { invoke, listen } from "./backend";
import { ago, nowSecs, openExternal } from "./util";

/** src-tauri/src/update.rs */
export interface UpdateStatus {
  version: string;
  build_channel: string;
  channel: string;
  channels: { name: string; warning: string | null }[];
  disabled: string | null;
  checking: boolean;
  checked_at: number | null;
  available: { version: string; notes_url: string; ready: boolean } | null;
  waiting_for_lands: string[];
  error: string | null;
}

const UPDATE_EVENT = "nitely://update";

/** The update status, kept current from the app's events. */
export function useUpdateStatus(): [UpdateStatus | null, (s: UpdateStatus) => void] {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  useEffect(() => {
    invoke<UpdateStatus>("update_status").then(setStatus).catch(() => setStatus(null));
    const unlisten = listen<UpdateStatus>(UPDATE_EVENT, (e) => setStatus(e.payload));
    return () => {
      unlisten.then((f) => f());
    };
  }, []);
  return [status, setStatus];
}

/** The sidebar's version line: quiet, and the one place an update says it is ready. */
export function UpdateLine({ status, onStatus }: { status: UpdateStatus | null; onStatus: (s: UpdateStatus) => void }) {
  if (!status) return null;
  const a = status.available;
  if (status.waiting_for_lands.length > 0) {
    return <div className="nav-update">Updating to {a?.version} once the land in {status.waiting_for_lands.join(", ")} finishes</div>;
  }
  if (a?.ready) {
    return (
      <div className="nav-update ready">
        <span>Nitely {a.version} is ready.</span>{" "}
        <button className="link" onClick={() => invoke<UpdateStatus>("restart_to_update").then(onStatus).catch(() => {})}>
          Restart to update
        </button>
        {" · "}
        <a href={a.notes_url} onClick={openExternal(a.notes_url)}>
          What's new
        </a>
      </div>
    );
  }
  return (
    <div className="nav-update" title={status.disabled ?? status.error ?? undefined}>
      Nitely {status.version} · {status.build_channel === "dev" ? "dev build" : status.channel}
      {a && !a.ready ? ` · downloading ${a.version}` : ""}
    </div>
  );
}

/** A banner when the app gave this data folder its own approval key (a profile on NITELY_HOME):
 * what was approved or paired there before has to be done again. It stays, at any width and
 * across restarts, until the person dismisses it; the dismissal is kept in that profile's own
 * database, so it never hides another profile's notice. */
export function ApprovalNotice() {
  const [notice, setNotice] = useState<{ text: string; at: number } | null>(null);
  useEffect(() => {
    invoke<{ text: string; at: number } | null>("approval_notice").then(setNotice).catch(() => setNotice(null));
  }, []);
  if (!notice) return null;
  const dismiss = () => {
    setNotice(null);
    invoke("dismiss_approval_notice").catch(() => {});
  };
  return (
    <p className="banner notice small" role="status">
      {notice.text}{" "}
      <button className="link" onClick={dismiss}>
        Got it
      </button>
    </p>
  );
}

/** The Updates card in Settings: the version, the channel to follow, and checking now. */
export function UpdatesCard({ status, onStatus, titled = true }: { status: UpdateStatus | null; onStatus: (s: UpdateStatus) => void; titled?: boolean }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  if (!status) return null;
  const run = async (work: () => Promise<UpdateStatus>) => {
    setBusy(true);
    setError(null);
    try {
      onStatus(await work());
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };
  const choose = (name: string) => {
    setPending(null);
    run(() => invoke<UpdateStatus>("set_update_channel", { channel: name }));
  };
  const warning = pending ? status.channels.find((c) => c.name === pending)?.warning : null;
  const a = status.available;

  return (
    <section className="settings-card updates">
      {titled && <h2>Updates</h2>}
      <p className="settings-why">
        Nitely {status.version}, {status.build_channel === "dev" ? "built from source" : `from the ${status.build_channel} channel`}. It checks for a
        newer version every few hours, downloads it in the background, and installs it only when you restart it or quit, never while a land
        runs. Agents keep working: the new <code>nitely</code> takes over from their next call.
      </p>
      {status.disabled ? (
        <p className="small muted">Not checking: {status.disabled}.</p>
      ) : (
        <>
          <div className="channel-choice" role="radiogroup" aria-label="Release channel">
            {status.channels.map((c) => (
              <label key={c.name} className={c.name === status.channel ? "on" : ""}>
                <input
                  type="radio"
                  name="channel"
                  checked={c.name === status.channel}
                  disabled={busy}
                  onChange={() => (c.warning ? setPending(c.name) : choose(c.name))}
                />
                <span className="channel-name">{c.name}</span>
                <span className="small muted">{CHANNEL_WORDS[c.name] ?? ""}</span>
              </label>
            ))}
          </div>
          {warning && pending && (
            <div className="channel-warning small" role="alert">
              <p>{warning}</p>
              <button className="btn" onClick={() => choose(pending)}>
                Follow {pending} anyway
              </button>{" "}
              <button className="btn" onClick={() => setPending(null)}>
                Stay on {status.channel}
              </button>
            </div>
          )}
          <div className="settings-row">
            <button className="btn" disabled={busy || status.checking} onClick={() => run(() => invoke<UpdateStatus>("check_for_update"))}>
              {status.checking ? "Checking…" : "Check now"}
            </button>
            <span className="small muted">
              {a ? (a.ready ? `${a.version} is ready; restart to update.` : `Downloading ${a.version}…`) : status.checked_at ? `Up to date, checked ${ago(nowSecs(), status.checked_at)}.` : ""}
            </span>
          </div>
        </>
      )}
      {(error ?? status.error) && <p className="error small">{error ?? status.error}</p>}
    </section>
  );
}

const CHANNEL_WORDS: Record<string, string> = {
  stable: "releases, tested as betas first",
  beta: "the next release, a few weeks early",
  nightly: "main as of last night; may break",
};
