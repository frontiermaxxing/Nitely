import { useEffect, useRef, useState } from "react";
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
  // The project on screen now: a pause answered after a switch must not land on another project.
  const shown = useRef(props.projectKey);
  shown.current = props.projectKey;

  // Another project's crew must not show while this one loads; a refresh of the same project keeps
  // the control mounted, so its button keeps focus.
  const loaded = useRef(props.projectKey);
  if (loaded.current !== props.projectKey) {
    loaded.current = props.projectKey;
    setCrew(null);
    setError(null);
  }

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
    const project = props.projectKey;
    const mine = () => shown.current === project;
    setBusy(true);
    invoke<CrewStatus | null>("crew_pause", { project, paused: !crew.paused })
      .then((c) => mine() && (setCrew(c), setError(null)))
      .catch((e) => mine() && setError(String(e)))
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
