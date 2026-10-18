import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Waits } from "./types";
import { WAIT_KIND, behindLine, held } from "./waits";
import "./styles/waits.css";

/** Reads every waiting request, again whenever the data changes (`version`). Null until read. */
export function useWaits(version: number): Waits | null {
  const [waits, setWaits] = useState<Waits | null>(null);
  useEffect(() => {
    let live = true;
    invoke<Waits>("waits_overview")
      .then((w) => live && setWaits(w))
      .catch(() => live && setWaits(null));
    return () => {
      live = false;
    };
  }, [version]);
  return waits;
}

/**
 * Every request waiting now on Now and Overview (NIT-322): a lane run, a submit, a review or a land,
 * its issue, why it waits and what it waits behind, longest waiting first. Nothing when nothing
 * waits.
 */
export function WaitingStrip(props: { waits: Waits | null; now: number; onOpenIssue: (key: string) => void }) {
  const list = props.waits?.waiting ?? [];
  if (list.length === 0) return null;
  return (
    <section className="waits-strip" aria-label="Waiting">
      <h2>
        Waiting <span className="head-note">{list.length === 1 ? "1 request" : `${list.length} requests`}, longest first</span>
      </h2>
      <ol className="wait-rows">
        {list.map((w, i) => (
          <li key={`${w.kind}-${w.issue ?? i}-${w.since}`} className={`wait-row wait-${w.kind}`}>
            <span className="wait-kind">{WAIT_KIND[w.kind] ?? w.kind}</span>
            {w.issue ? (
              <button className="link mono wait-key" onClick={() => props.onOpenIssue(w.issue!)}>
                {w.issue}
              </button>
            ) : (
              <span className="mono muted wait-key">{w.project ?? "—"}</span>
            )}
            <span className="wait-why">{w.why}</span>
            <span className="wait-age" title={`Waiting since ${new Date(w.since * 1000).toLocaleString()}`}>
              {held(props.now, w.since)}
            </span>
            {w.behind.length > 0 && (
              <span className="wait-behind">
                behind{" "}
                {w.behind.map((b, j) => (
                  <span key={j}>
                    {j > 0 && "; "}
                    {b.issue ? (
                      <button className="link" onClick={() => props.onOpenIssue(b.issue!)}>
                        {behindLine(b, props.now)}
                      </button>
                    ) : (
                      behindLine(b, props.now)
                    )}
                  </span>
                ))}
              </span>
            )}
          </li>
        ))}
      </ol>
    </section>
  );
}
