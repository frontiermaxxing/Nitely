import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { crewTitle, crewWords } from "./crew";
import type { CrewStatus } from "./types";

/**
 * The project's crew in its header (docs/crew.md): how many workers run of the cap, why it is not
 * working when it is not, and pause or resume. Nothing when the project has no crew.
 */
export function CrewControl(props: { projectKey: string; version: number; now: number }) {
  const [crew, setCrew] = useState<CrewStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    invoke<CrewStatus | null>("crew_status", { project: props.projectKey })
      .then((c) => live && (setCrew(c), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.projectKey, props.version]);

  if (!crew) return error ? <span className="crew error small" title={error}>Crew unavailable</span> : null;
  const words = crewWords(crew);
  const toggle = () => {
    setBusy(true);
    invoke<CrewStatus | null>("crew_pause", { project: props.projectKey, paused: !crew.paused })
      .then((c) => (setCrew(c), setError(null)))
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  return (
    <div className="crew" title={crewTitle(crew, props.now)}>
      <span className="crew-count">
        Crew <span className="mono">{crew.running.length}/{crew.cap}</span>
      </span>
      {words && <span className={`crew-state${words.attention ? " attention-ink" : " muted"}`}>{words.text}</span>}
      {error && <span className="error small">{error}</span>}
      <button className="btn quiet" onClick={toggle} disabled={busy}>
        {crew.paused ? "Resume" : "Pause"}
      </button>
    </div>
  );
}
